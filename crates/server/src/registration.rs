//! Self-service account registration — Mastodon's
//! `AppSignUpService` + the `User` model's sign-up lifecycle.
//!
//! E-mail is optional. A sign-up that provides an address (on a server that
//! can send mail) creates an *unconfirmed* user and e-mails a confirmation
//! link; confirming the address (and, in `approved` mode, an admin approving
//! the account) makes the login functional. A sign-up without an address —
//! or on a server with no `[smtp]` config — has nothing to confirm, so the
//! user starts confirmed and only the approval gate (if any) remains. The
//! moment a login becomes functional, the welcome mail goes out (when
//! possible), staff get their `admin.sign_up` notification and the
//! `account.approved` webhook fires (Mastodon's `prepare_new_user!`).

use std::net::IpAddr;

use plamenu_ap::acct::Acct;
use plamenu_db::account::{self, Account, NewLocalAccount};
use plamenu_db::instance_settings::RegistrationsMode;
use plamenu_db::user::{self, NewRegisteredUser, User};
use plamenu_db::{
    instance_policy, instance_settings, invite, notification, role, username_block, webhook,
};
use serde_json::{Value, json};

use fluent_bundle::FluentArgs;

use crate::error::ApiError;
use crate::web::i18n::Locale;
use crate::{AppState, auth, mailer};

/// Mastodon's local-username length cap (`Account::USERNAME_LENGTH_LIMIT`).
pub(crate) const USERNAME_LENGTH_LIMIT: usize = 30;
/// Mastodon's `RegistrationFormTimeValidator` doesn't apply (no form clock),
/// but the invite-request `text` cap does (`UserInviteRequest`).
pub(crate) const REASON_LENGTH_LIMIT: usize = 420;

pub struct SignUpParams<'a> {
    pub username: &'a str,
    /// Optional: without an address (or without a mail relay) the account is
    /// created with nothing pending confirmation.
    pub email: Option<&'a str>,
    pub password: &'a str,
    pub agreement: bool,
    pub locale: Option<&'a str>,
    /// The "why do you want to join" text shown to admins in approval mode.
    pub reason: Option<&'a str>,
    /// An invite code. While the invite is valid it bypasses the
    /// closed/approval registration gates and grants approval upfront; an
    /// unknown or dead code is treated as absent, like Mastodon.
    pub invite_code: Option<&'a str>,
    /// The owner's IANA time zone, already normalised to a known
    /// identifier (or `None`) by the caller.
    pub time_zone: Option<&'a str>,
    /// The submitted date of birth (`YYYY-MM-DD`), used only when the server has
    /// an age gate. Never persisted — validated then discarded.
    pub date_of_birth: Option<&'a str>,
}

/// One field's validation failure — serialized in Mastodon's
/// `ValidationErrorFormatter` shape.
struct FieldError {
    attribute: &'static str,
    code: &'static str,
    description: String,
}

fn field_error(attribute: &'static str, code: &'static str, description: &str) -> FieldError {
    FieldError {
        attribute,
        code,
        description: description.to_owned(),
    }
}

/// Renders field errors as Mastodon's 422: `Validation failed: …` plus the
/// per-attribute `details` array clients parse.
fn validation_error(errors: &[FieldError]) -> ApiError {
    fn human(attribute: &str) -> &str {
        match attribute {
            "username" => "Username",
            "email" => "E-mail address",
            "password" => "Password",
            "agreement" => "Agreement",
            "reason" => "Reason",
            other => other,
        }
    }
    let message = errors
        .iter()
        .map(|e| format!("{} {}", human(e.attribute), e.description))
        .collect::<Vec<_>>()
        .join(", ");
    let mut details = serde_json::Map::new();
    for error in errors {
        details
            .entry(error.attribute.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("details entries are arrays")
            .push(json!({ "error": error.code, "description": error.description }));
    }
    ApiError::Validation {
        message: format!("Validation failed: {message}"),
        details: Value::Object(details),
    }
}

/// Mastodon's `check_enabled_registrations` / `NotPermittedError` rendering.
fn not_allowed() -> ApiError {
    ApiError::Forbidden("This action is not allowed".into())
}

/// The age gate. `min_age == 0` is off: returns `None` and ignores the
/// date of birth. Otherwise the date must be present, valid and at least
/// `min_age` years ago; on success returns the verification timestamp to stamp.
fn verify_age(
    min_age: i32,
    date_of_birth: Option<&str>,
) -> Result<Option<time::OffsetDateTime>, ApiError> {
    if min_age <= 0 {
        return Ok(None);
    }
    let dob = date_of_birth
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            validation_error(&[field_error("date_of_birth", "ERR_BLANK", "can't be blank")])
        })?;
    let format = time::format_description::parse("[year]-[month]-[day]")
        .expect("static date format description");
    let dob = time::Date::parse(dob, &format).map_err(|_| {
        validation_error(&[field_error("date_of_birth", "ERR_INVALID", "is invalid")])
    })?;
    let today = time::OffsetDateTime::now_utc().date();
    if !old_enough(dob, min_age, today) {
        return Err(validation_error(&[field_error(
            "date_of_birth",
            "ERR_INVALID",
            "is invalid",
        )]));
    }
    Ok(Some(time::OffsetDateTime::now_utc()))
}

/// Whether someone born on `dob` is at least `min_age` full years old as of
/// `today` — the birthday must already have passed this year.
fn old_enough(dob: time::Date, min_age: i32, today: time::Date) -> bool {
    let mut age = today.year() - dob.year();
    if (u8::from(today.month()), today.day()) < (u8::from(dob.month()), dob.day()) {
        age -= 1;
    }
    age >= min_age
}

/// Whether self-service sign-up is currently possible at all. E-mail being
/// unconfigured no longer closes the gate: confirmation is simply skipped.
pub async fn open_for_registrations(state: &AppState) -> Result<bool, ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    Ok(settings.registrations_mode() != RegistrationsMode::None)
}

/// Performs a sign-up: gate checks, validation, account + unconfirmed user
/// creation, the confirmation e-mail and the `account.created` webhook.
/// Returns the created user (its token/session is the caller's concern).
#[allow(
    clippy::explicit_auto_deref,
    clippy::too_many_lines,
    reason = "the signup transaction includes account, user, key, and policy provisioning"
)]
pub async fn sign_up(
    state: &AppState,
    app_id: i64,
    remote_ip: Option<IpAddr>,
    params: SignUpParams<'_>,
) -> Result<User, ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    let mode = settings.registrations_mode();
    // A currently-valid invite opens the gate on its own — Mastodon's
    // `allowed_registration?` is `registrations_open? || invite&.valid_for_use?`.
    let invite = match params.invite_code.map(str::trim).filter(|c| !c.is_empty()) {
        Some(code) => invite::find_valid_by_code(&state.pool, code).await?,
        None => None,
    };
    if mode == RegistrationsMode::None && invite.is_none() {
        return Err(not_allowed());
    }
    // `sign_up_block` IP severity — Mastodon's `ip_blocked?`.
    if ip_blocked_for_sign_up(state, remote_ip, "sign_up_block").await? {
        return Err(not_allowed());
    }

    let email = params.email.map(str::trim).filter(|e| !e.is_empty());
    validate(state, &params, email).await?;

    // Age gate: when the server sets a positive `min_age`, the sign-up
    // must carry a date of birth at least that old. `age_verified_at` is stamped
    // on success; the date itself is never stored. Off by default (min_age = 0),
    // in which case `date_of_birth` is ignored entirely.
    let age_verified_at = verify_age(settings.min_age, params.date_of_birth)?;

    // `sign_up_requires_approval` IP blocks and approval-mode username
    // reservations force the admin queue even in open mode; a valid invite
    // grants approval outright (Mastodon's `grant_approval?`).
    let approval_forced = ip_blocked_for_sign_up(state, remote_ip, "sign_up_requires_approval")
        .await?
        || username_block::matches(&state.pool, params.username, true).await?;
    let approved = (mode == RegistrationsMode::Open && !approval_forced) || invite.is_some();

    // Everything CPU-heavy is prepared *before* the transaction — the RSA and
    // Ed25519 key material, the Argon2 password hash, and the confirmation
    // token/hash — so the transaction never spans crypto work.
    // Argon2 and RSA generation run on the blocking pool behind the shared
    // credential-crypto gate so a signup burst can't monopolize the runtime;
    // Ed25519 generation is cheap and stays inline.
    let keypair = auth::generate_keypair_gated().await?;
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let password_hash = auth::hash_password_gated(params.password.to_owned()).await?;
    // A confirmation link only makes sense with an address to send it to and
    // a relay to send it through; otherwise the user starts confirmed.
    let confirmation_token =
        (email.is_some() && mailer::enabled(state)).then(auth::generate_secret);
    let confirmation_token_hash = confirmation_token.as_deref().map(auth::hash_secret);

    // One transaction commits the whole accepted signup: the account row, its
    // Ed25519 keys, the user (and any invite-use increment), and — when the
    // account must confirm by e-mail — the confirmation-mail job. A failure
    // anywhere before commit rolls all of it back, so a reserved username or
    // e-mail is released rather than left shadowing a userless account, and no
    // signup returns an error while a half-made account survives. Because the
    // confirmation mail commits with the account, a success
    // response always means the user can actually confirm.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let created = account::create_local_immutable(
        &mut *tx,
        NewLocalAccount {
            username: params.username,
            display_name: "",
            note: "",
            public_key_pem: &keypair.public_pem,
        },
        &state.config.domain,
    )
    .await
    .map_err(|err| match err {
        plamenu_db::DbError::UsernameTaken => validation_error(&[field_error(
            "username",
            "ERR_TAKEN",
            "has already been taken",
        )]),
        other => other.into(),
    })?;
    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        ))
    })?;
    crate::key_store::provision_account_tx(
        &mut *tx,
        keyring,
        &state.config.domain,
        &created,
        &keypair,
        &ed25519,
    )
    .await
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let user = user::create_registered_conn(
        &mut *tx,
        NewRegisteredUser {
            account_id: created.id,
            email,
            password_hash: &password_hash,
            approved,
            confirmation_token_hash: confirmation_token_hash.as_deref(),
            locale: params.locale,
            sign_up_ip: remote_ip.map(|ip| ip.to_string()).as_deref(),
            created_by_application_id: app_id,
            invite_request_text: params.reason.filter(|r| !r.trim().is_empty()),
            invite_id: invite.as_ref().map(|i| i.id),
            time_zone: params.time_zone,
            age_verified_at,
        },
    )
    .await
    .map_err(|err| match err {
        // Raced past the upfront check: the whole transaction rolls back, so
        // the account just inserted is undone and the username is freed.
        plamenu_db::DbError::EmailTaken => {
            validation_error(&[field_error("email", "ERR_TAKEN", "has already been taken")])
        }
        other => other.into(),
    })?;
    if let (Some(token), Some(recipient)) = (&confirmation_token, email) {
        // The confirmation mail is the one *required* durable side effect: a
        // confirming account is inert until it is delivered, so it commits with
        // the account instead of racing it afterward.
        // The applicant's own locale (seeded from their `Accept-Language` on
        // this very request) — mail is read outside it, so it is stored first
        // and read back rather than negotiated per message.
        let locale = Locale::negotiate(params.locale, None);
        let (subject, body) =
            render_confirmation_email(state, &settings.site_title, &created, token, locale);
        plamenu_db::email::enqueue_tx(
            &mut tx,
            &plamenu_db::email::OutgoingEmail {
                recipient,
                subject: &subject,
                body: &body,
            },
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;

    // Side effects run only after the signup has durably committed.
    deliver_signup_side_effects(state, &user, confirmation_token.is_some()).await;
    crate::webhooks::account_event(state, webhook::ACCOUNT_CREATED, created.id).await;
    Ok(user)
}

/// Best-effort notifications run *after* the signup transaction commits (QC
/// audit #41). The account already exists durably, so a transient failure here
/// is logged rather than propagated — the former `?` on these made a lost
/// welcome mail or staff ping fail the whole signup while leaving the account
/// behind. Each notification is idempotent. Skipped entirely when the account
/// must first confirm by e-mail, since the confirmation mail committed with the
/// account and the functional/staff pings fire later, at confirmation time.
async fn deliver_signup_side_effects(state: &AppState, user: &User, awaiting_confirmation: bool) {
    if awaiting_confirmation {
        return;
    }
    if user.approved {
        // Nothing to confirm and nothing pending approval: the login is
        // functional from the first moment.
        if let Err(error) = user_became_functional(state, user).await {
            tracing::warn!(error = %error.chain(), "post-signup welcome/notify failed");
        }
    } else {
        if let Err(error) = notify_staff_about_sign_up(state, user).await {
            tracing::warn!(error = %error.chain(), "post-signup staff sign-up notify failed");
        }
        if let Err(error) = notify_staff_about_pending_account(state, user).await {
            tracing::warn!(error = %error.chain(), "post-signup pending-account mail failed");
        }
    }
}

async fn validate(
    state: &AppState,
    params: &SignUpParams<'_>,
    email: Option<&str>,
) -> Result<(), ApiError> {
    let mut errors = Vec::new();

    if !params.agreement {
        errors.push(field_error("agreement", "ERR_ACCEPTED", "must be accepted"));
    }

    let username = params.username;
    if username.is_empty() {
        errors.push(field_error("username", "ERR_BLANK", "can't be blank"));
    } else if username.chars().count() > USERNAME_LENGTH_LIMIT {
        errors.push(field_error(
            "username",
            "ERR_TOO_LONG",
            &format!("is too long (maximum is {USERNAME_LENGTH_LIMIT} characters)"),
        ));
    } else if Acct::new(username, &state.config.domain).is_err() {
        errors.push(field_error("username", "ERR_INVALID", "is invalid"));
    } else if account::local_username_reserved(&state.pool, username).await? {
        errors.push(field_error(
            "username",
            "ERR_TAKEN",
            "has already been taken",
        ));
    } else if username_block::matches(&state.pool, username, false).await? {
        // The admin username blocklist (Mastodon's `UnreservedUsernameValidator`).
        errors.push(field_error("username", "ERR_RESERVED", "is reserved"));
    }

    // E-mail is optional; only a *provided* address is validated.
    if let Some(email) = email {
        if !plausible_email(email) {
            errors.push(field_error("email", "ERR_INVALID", "is invalid"));
        } else if email_blocked(state, email).await? {
            // Mastodon's `EmailAddressValidation` against email_domain_blocks /
            // canonical_email_blocks (`ERR_BLOCKED`).
            errors.push(field_error("email", "ERR_BLOCKED", "is not allowed"));
        } else if user::find_by_email(&state.pool, email).await?.is_some() {
            errors.push(field_error("email", "ERR_TAKEN", "has already been taken"));
        }
    }

    // The length policy lives in `auth`; registration keeps
    // Mastodon's field-error codes but no longer owns the numbers.
    match auth::validate_password(params.password) {
        Ok(()) => {}
        Err(auth::PasswordPolicy::Empty) => {
            errors.push(field_error("password", "ERR_BLANK", "can't be blank"));
        }
        Err(auth::PasswordPolicy::TooShort) => {
            errors.push(field_error(
                "password",
                "ERR_TOO_SHORT",
                &format!(
                    "is too short (minimum is {} characters)",
                    auth::PASSWORD_MIN
                ),
            ));
        }
        Err(auth::PasswordPolicy::TooLong) => {
            errors.push(field_error(
                "password",
                "ERR_TOO_LONG",
                &format!("is too long (maximum is {} characters)", auth::PASSWORD_MAX),
            ));
        }
    }

    if let Some(reason) = params.reason
        && reason.chars().count() > REASON_LENGTH_LIMIT
    {
        errors.push(field_error(
            "reason",
            "ERR_TOO_LONG",
            &format!("is too long (maximum is {REASON_LENGTH_LIMIT} characters)"),
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(validation_error(&errors))
    }
}

/// Deliberately loose (Devise's own regex is just `@` with something around
/// it); real verification is the confirmation mail itself.
fn plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

async fn email_blocked(state: &AppState, email: &str) -> Result<bool, ApiError> {
    Ok(
        crate::instance_policy::ensure_email_login_allowed(&state.pool, email)
            .await
            .is_err(),
    )
}

async fn ip_blocked_for_sign_up(
    state: &AppState,
    remote_ip: Option<IpAddr>,
    severity: &str,
) -> Result<bool, ApiError> {
    let Some(remote_ip) = remote_ip else {
        return Ok(false);
    };
    let blocks = instance_policy::active_ip_blocks(&state.pool).await?;
    Ok(blocks.iter().any(|block| {
        block.severity == severity && crate::instance_policy::ip_matches_cidr(remote_ip, &block.ip)
    }))
}

/// Renders the confirmation instructions (subject, plain-text body) without
/// touching the database, so a resend can render *before* rotating the token
/// and then commit both in one transaction.
#[must_use]
pub fn render_confirmation_email(
    state: &AppState,
    site_title: &str,
    account: &Account,
    confirmation_token: &str,
    locale: Locale,
) -> (String, String) {
    let domain = &state.config.domain;
    let link =
        format!("https://{domain}/auth/confirmation?confirmation_token={confirmation_token}");
    let mut args = FluentArgs::new();
    args.set("site", site_title.to_owned());
    let subject = locale.plain_with("email-confirm-subject", &args);
    args.set("username", account.username.clone());
    args.set("url", format!("https://{domain}"));
    let body = format!(
        "{greeting}\n\n{intro}\n\n{hint}\n\n{link}\n\n{ignore}\n",
        greeting = locale.plain_with("email-confirm-greeting", &args),
        intro = locale.plain_with("email-confirm-intro", &args),
        hint = locale.plain("email-confirm-link-hint"),
        ignore = locale.plain("email-confirm-ignore"),
    );
    (subject, body)
}

/// Confirms the address behind a raw confirmation token. `None` = unknown or
/// already-used token. Runs the became-functional side effects when the
/// account is (or simultaneously became) approved, and the staff-pending
/// notification otherwise.
pub async fn confirm_by_token(state: &AppState, raw_token: &str) -> Result<Option<User>, ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    // Mastodon's `grant_approval_on_confirmation?`: a sign-up from before
    // the server switched to open registrations still gets approved.
    let grant_approval = settings.registrations_mode() == RegistrationsMode::Open;
    let Some(user) =
        user::confirm_by_token_hash(&state.pool, &auth::hash_secret(raw_token), grant_approval)
            .await?
    else {
        return Ok(None);
    };
    if user.approved {
        user_became_functional(state, &user).await?;
    } else {
        notify_staff_about_sign_up(state, &user).await?;
        notify_staff_about_pending_account(state, &user).await?;
    }
    Ok(Some(user))
}

/// Mastodon's `prepare_new_user!`: the moment a sign-up becomes a usable
/// login (confirmed + approved) — welcome mail, staff `admin.sign_up`
/// notifications, `account.approved` webhook. Call sites: confirmation (when
/// already/simultaneously approved) and admin approval (when confirmed).
pub async fn user_became_functional(state: &AppState, user: &User) -> Result<(), ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    let domain = &state.config.domain;
    if let Some(account) = account::find_by_id(&state.pool, user.account_id).await? {
        let locale = Locale::for_user(&state.pool, user.id).await?;
        let mut args = FluentArgs::new();
        args.set("site", settings.site_title.clone());
        args.set("username", account.username.clone());
        args.set("url", format!("https://{domain}"));
        args.set("login", format!("https://{domain}/login"));
        let subject = locale.plain_with("email-welcome-subject", &args);
        let body = format!(
            "{greeting}\n\n{body}\n",
            greeting = locale.plain_with("email-confirm-greeting", &args),
            body = locale.plain_with("email-welcome-body", &args),
        );
        // Best-effort: a failed welcome mail must not undo the approval.
        // Users without an address simply get no welcome mail.
        if mailer::enabled(state)
            && let Some(recipient) = user.email.as_deref()
            && let Err(error) = mailer::enqueue(state, recipient, &subject, &body).await
        {
            tracing::warn!(error = %error.chain(), "failed to enqueue welcome e-mail");
        }
    }
    notify_staff_about_sign_up(state, user).await?;
    crate::webhooks::account_event(state, webhook::ACCOUNT_APPROVED, user.account_id).await;
    Ok(())
}

/// Emits the Mastodon-compatible `admin.sign_up` in-app notification to every
/// staff member who can manage users — the same signal Mastodon, `GoToSocial`
/// and friends surface in existing clients (streaming, Web Push,
/// `GET /api/v1/notifications`). Fires at most once per applicant, at the
/// earliest of {awaiting review, becoming functional}: an approval-mode
/// sign-up pings staff the moment it is reviewable, an open-mode sign-up pings
/// once it is usable, and a pending account that is later approved does not
/// notify twice (`notification::sign_up_exists` is the guard). Best-effort per
/// recipient, like the rest of `user_became_functional`.
async fn notify_staff_about_sign_up(state: &AppState, user: &User) -> Result<(), ApiError> {
    notify_staff_about_account_signup(state, user.account_id).await
}

/// Staff sign-up notification for any account entering the local namespace.
/// Portable accounts have no `users` row, but they are still registrations an
/// administrator must be able to discover and moderate.
pub(crate) async fn notify_staff_about_account_signup(
    state: &AppState,
    account_id: i64,
) -> Result<(), ApiError> {
    if notification::sign_up_exists(&state.pool, account_id).await? {
        return Ok(());
    }
    for staff_account_id in
        role::account_ids_who_can(&state.pool, role::permission::MANAGE_USERS).await?
    {
        if staff_account_id != account_id {
            notification::create(
                &state.pool,
                staff_account_id,
                account_id,
                "admin.sign_up",
                None,
            )
            .await?;
        }
    }
    Ok(())
}

/// Mastodon's `notify_staff_about_pending_account!` — mails every staff
/// member who can manage users that a sign-up awaits review.
async fn notify_staff_about_pending_account(state: &AppState, user: &User) -> Result<(), ApiError> {
    if !mailer::enabled(state) {
        return Ok(());
    }
    let settings = instance_settings::get(&state.pool).await?;
    let Some(account) = account::find_by_id(&state.pool, user.account_id).await? else {
        return Ok(());
    };
    let domain = &state.config.domain;
    let mut args = FluentArgs::new();
    args.set("site", settings.site_title.clone());
    args.set("username", account.username.clone());
    args.set(
        "url",
        format!("https://{domain}/admin/accounts/{}", account.id),
    );
    for staff_account_id in
        role::account_ids_who_can(&state.pool, role::permission::MANAGE_USERS).await?
    {
        if let Some(staff) = user::find_by_account_id(&state.pool, staff_account_id).await?
            && let Some(staff_email) = staff.email.as_deref()
        {
            // Each moderator reads this in their own interface language.
            let locale = Locale::for_user(&state.pool, staff.id).await?;
            args.set(
                "email",
                user.email
                    .clone()
                    .unwrap_or_else(|| locale.plain("email-pending-no-address")),
            );
            let subject = locale.plain_with("email-pending-subject", &args);
            let body = format!(
                "{body}\n\n{hint}\n",
                body = locale.plain_with("email-pending-body", &args),
                hint = locale.plain_with("email-pending-link-hint", &args),
            );
            if let Err(error) = mailer::enqueue(state, staff_email, &subject, &body).await {
                tracing::warn!(error = %error.chain(), "failed to enqueue pending-account e-mail");
            }
        }
    }
    Ok(())
}
