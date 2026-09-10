use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use plamenu_db::{oauth, two_factor, user};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::{PasswordLoginError, generate_secret, hash_secret};
use crate::instance_policy::RemoteIp;

use super::auth::LemmyUser;
use super::error::LemmyError;

const CLIENT_ID: &str = "plamenu-lemmy-api-v3";
const SCOPES: &str = "read write follow push admin:read admin:write";

#[derive(Deserialize)]
pub struct Login {
    username_or_email: String,
    password: String,
    totp_2fa_token: Option<String>,
}

pub(super) async fn compatibility_app(state: &AppState) -> Result<oauth::App, LemmyError> {
    if let Some(app) = oauth::find_app_by_client_id(&state.pool, CLIENT_ID).await? {
        return Ok(app);
    }
    let secret = generate_secret();
    match oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Plamenu Lemmy API v3 compatibility",
            website: Some("https://join-lemmy.org/docs/contributors/04-api.html"),
            client_id: CLIENT_ID,
            client_secret_hash: &hash_secret(&secret),
            redirect_uris: &[],
            scopes: SCOPES,
        },
    )
    .await
    {
        Ok(app) => Ok(app),
        // A concurrent first login may have won the unique-client-id insert.
        Err(_) => oauth::find_app_by_client_id(&state.pool, CLIENT_ID)
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::INTERNAL_SERVER_ERROR, "unknown")),
    }
}

async fn issue_token(state: &AppState, user_id: i64) -> Result<String, LemmyError> {
    let app = compatibility_app(state).await?;
    let raw = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&raw),
        app.id,
        Some(user_id),
        SCOPES,
    )
    .await?;
    Ok(raw)
}

fn login_error(error: PasswordLoginError) -> LemmyError {
    match error {
        PasswordLoginError::Locked => LemmyError::unauthorized("locked"),
        PasswordLoginError::Unconfirmed => LemmyError::unauthorized("email_not_verified"),
        PasswordLoginError::PendingApproval => {
            LemmyError::unauthorized("registration_application_pending")
        }
        PasswordLoginError::Disabled => LemmyError::unauthorized("site_ban"),
        PasswordLoginError::EmailBlocked | PasswordLoginError::BadCredentials => {
            LemmyError::unauthorized("incorrect_login")
        }
    }
}

async fn verify_second_factor(
    state: &AppState,
    user: &user::User,
    code: Option<&str>,
) -> Result<&'static str, LemmyError> {
    if !user.otp_required_for_login {
        return Ok("password");
    }
    let code = code
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| LemmyError::unauthorized("missing_totp_token"))?;
    if let Some(secret_box) = crate::crypto::otp_box(&state.config)
        && let Some(encrypted) = user.otp_secret.as_deref()
        && let Some(secret) = secret_box.decrypt(encrypted)
    {
        let secret = String::from_utf8_lossy(&secret);
        let now = time::OffsetDateTime::now_utc()
            .unix_timestamp()
            .max(0)
            .cast_unsigned();
        if let Some(step) = crate::totp::verify(&secret, code, now) {
            if user::consume_otp_timestep(&state.pool, user.id, step).await? {
                return Ok("otp");
            }
            return Err(LemmyError::unauthorized("incorrect_totp_token"));
        }
    }
    let recovery = code.split_whitespace().collect::<String>().to_lowercase();
    if !recovery.is_empty()
        && two_factor::consume_backup_code(&state.pool, user.id, &hash_secret(&recovery)).await?
    {
        return Ok("otp");
    }
    Err(LemmyError::unauthorized("incorrect_totp_token"))
}

pub async fn login(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    Json(form): Json<Login>,
) -> Result<Json<Value>, LemmyError> {
    crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip)
        .await
        .map_err(LemmyError::from)?;
    crate::rate_limit::check_email(
        &state,
        crate::rate_limit::Bucket::LoginAttemptsEmail,
        &form.username_or_email,
    )
    .await
    .map_err(LemmyError::from)?;
    let ip = remote_ip.map(|ip| ip.to_string());
    let user_agent = crate::auth::user_agent_string(&headers);
    let user = crate::auth::authenticate_password(
        &state,
        &form.username_or_email,
        form.password,
        ip.as_deref(),
        user_agent.as_deref(),
    )
    .await
    .map_err(login_error)?;
    let method = verify_second_factor(&state, &user, form.totp_2fa_token.as_deref()).await?;
    let app = compatibility_app(&state).await?;
    let locale = crate::auth::accept_language_primary(&headers);
    let _ = crate::sign_in::record(
        &state,
        user.id,
        locale.as_deref(),
        ip.as_deref(),
        user::SignInContext {
            method: Some(method),
            user_agent: user_agent.as_deref(),
        },
    )
    .await;
    let raw = generate_secret();
    oauth::create_token_with_meta(
        &state.pool,
        &hash_secret(&raw),
        app.id,
        Some(user.id),
        SCOPES,
        oauth::SessionMeta {
            user_agent: user_agent.as_deref(),
            ip: ip.as_deref(),
        },
    )
    .await?;
    Ok(Json(json!({
        "jwt": raw,
        "registration_created": false,
        "verify_email_sent": false,
    })))
}

pub async fn logout(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    headers: HeaderMap,
) -> Result<Json<Value>, LemmyError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| LemmyError::unauthorized("not_logged_in"))?;
    oauth::revoke_token(&state.pool, &hash_secret(raw), current.app_id).await?;
    Ok(Json(json!({ "success": true })))
}

pub async fn validate_auth(LemmyUser(_): LemmyUser) -> Json<Value> {
    Json(json!({ "success": true }))
}

/// Lemmy's navbar badge counts, backed by Plamenu's notification marker and
/// direct-conversation read state.
pub async fn unread_count(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("read").map_err(LemmyError::from)?;
    let counts = plamenu_db::notification::lemmy_unread_counts(
        &state.pool,
        current.user.id,
        current.account.id,
    )
    .await?;
    Ok(Json(json!({
        "replies": counts.replies,
        "mentions": counts.mentions,
        "private_messages": counts.private_messages,
    })))
}

#[derive(Deserialize)]
pub struct Register {
    username: String,
    password: String,
    password_verify: String,
    email: Option<String>,
    answer: Option<String>,
    honeypot: Option<String>,
    #[allow(dead_code)]
    show_nsfw: Option<bool>,
}

pub async fn register(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    Json(form): Json<Register>,
) -> Result<Json<Value>, LemmyError> {
    if form.password != form.password_verify {
        return Err(LemmyError::bad_request("passwords_dont_match"));
    }
    if form
        .honeypot
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return Err(LemmyError::bad_request("honeypot_failed"));
    }
    let app = compatibility_app(&state).await?;
    let locale = crate::auth::accept_language_primary(&headers);
    let created = crate::registration::sign_up(
        &state,
        app.id,
        remote_ip,
        crate::registration::SignUpParams {
            username: &form.username,
            email: form.email.as_deref(),
            password: &form.password,
            agreement: true,
            locale: locale.as_deref(),
            reason: form.answer.as_deref(),
            invite_code: None,
            time_zone: None,
            date_of_birth: None,
        },
    )
    .await
    .map_err(LemmyError::from)?;
    let jwt = if created.functional() {
        Some(issue_token(&state, created.id).await?)
    } else {
        None
    };
    Ok(Json(json!({
        "jwt": jwt,
        "registration_created": true,
        "verify_email_sent": created.email.is_some() && !created.confirmed(),
    })))
}

#[derive(Deserialize)]
pub struct PasswordReset {
    email: String,
}

/// Paranoid-mode reset request: the same success response for known and
/// unknown addresses, with token creation and mail enqueue kept atomic.
pub async fn password_reset(
    State(state): State<AppState>,
    Json(form): Json<PasswordReset>,
) -> Result<Json<Value>, LemmyError> {
    let email = form.email.trim();
    crate::rate_limit::check_email(
        &state,
        crate::rate_limit::Bucket::PasswordResetsEmail,
        email,
    )
    .await
    .map_err(LemmyError::from)?;
    if crate::mailer::enabled(&state) {
        let task_state = state.clone();
        let address = email.to_owned();
        tokio::spawn(async move {
            if let Ok(Some(user)) = user::find_by_email(&task_state.pool, &address).await
                && let Some(recipient) = user.email.clone()
            {
                let token = generate_secret();
                if let Err(error) = crate::web::password::store_reset_token_and_send_email(
                    &task_state,
                    user.id,
                    &recipient,
                    &token,
                )
                .await
                {
                    tracing::error!(%error, "Lemmy password reset enqueue failed");
                }
            }
        });
    }
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct PasswordChangeAfterReset {
    token: String,
    password: String,
    password_verify: String,
}

pub async fn password_change_after_reset(
    State(state): State<AppState>,
    Json(form): Json<PasswordChangeAfterReset>,
) -> Result<Json<Value>, LemmyError> {
    if form.password != form.password_verify {
        return Err(LemmyError::bad_request("passwords_dont_match"));
    }
    crate::auth::validate_password(&form.password)
        .map_err(|_| LemmyError::bad_request("invalid_password"))?;
    let token_hash = hash_secret(&form.token);
    if user::find_by_reset_password_token_hash(&state.pool, &token_hash)
        .await?
        .is_none()
    {
        return Err(LemmyError::bad_request("invalid_reset_token"));
    }
    let password_hash = crate::auth::hash_password_gated(form.password)
        .await
        .map_err(LemmyError::from)?;
    if user::reset_password_by_token_and_revoke(&state.pool, &token_hash, &password_hash)
        .await?
        .is_none()
    {
        return Err(LemmyError::bad_request("invalid_reset_token"));
    }
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct ChangePassword {
    new_password: String,
    new_password_verify: String,
    old_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<ChangePassword>,
) -> Result<Json<Value>, LemmyError> {
    if form.new_password != form.new_password_verify {
        return Err(LemmyError::bad_request("passwords_dont_match"));
    }
    if !crate::auth::verify_password_gated(form.old_password, current.user.password_hash.clone())
        .await
    {
        return Err(LemmyError::unauthorized("incorrect_login"));
    }
    crate::auth::validate_password(&form.new_password)
        .map_err(|_| LemmyError::bad_request("invalid_password"))?;
    let password_hash = crate::auth::hash_password_gated(form.new_password)
        .await
        .map_err(LemmyError::from)?;
    user::change_password_and_revoke(&state.pool, current.user.id, &password_hash)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_person"))?;
    let jwt = issue_token(&state, current.user.id).await?;
    Ok(Json(json!({
        "jwt": jwt,
        "registration_created": false,
        "verify_email_sent": false,
    })))
}

#[derive(Deserialize)]
pub struct DeleteAccount {
    password: String,
    delete_content: bool,
}

pub async fn delete_account(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<DeleteAccount>,
) -> Result<Json<Value>, LemmyError> {
    if !form.delete_content {
        return Err(LemmyError::bad_request("delete_content_required"));
    }
    if !crate::auth::verify_password_gated(form.password, current.user.password_hash.clone()).await
    {
        return Err(LemmyError::unauthorized("incorrect_login"));
    }
    crate::moderation::self_delete_account(&state, &current.account)
        .await
        .map_err(LemmyError::from)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Default, Deserialize)]
pub struct SaveUserSettings {
    interface_language: Option<String>,
    avatar: Option<String>,
    banner: Option<String>,
    display_name: Option<String>,
    email: Option<String>,
    bio: Option<String>,
    bot_account: Option<bool>,
    discussion_languages: Option<Vec<i32>>,
}

pub async fn save_settings(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(raw): Json<Value>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let form: SaveUserSettings =
        serde_json::from_value(raw.clone()).map_err(|_| LemmyError::bad_request("invalid_form"))?;
    let current_avatar = crate::entities::avatar_url(&state.config.domain, &current.account, false);
    let current_banner = crate::entities::header_url(&state.config.domain, &current.account, false);
    let clear_avatar = form.avatar.as_deref() == Some("") && current_avatar.is_some();
    let clear_banner = form.banner.as_deref() == Some("") && current_banner.is_some();
    let avatar = match form.avatar.as_deref() {
        Some(url) if !url.is_empty() && Some(url) != current_avatar.as_deref() => {
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_profile_image_url"))?
                .into()
        }
        _ => None,
    };
    let banner = match form.banner.as_deref() {
        Some(url) if !url.is_empty() && Some(url) != current_banner.as_deref() => {
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_profile_image_url"))?
                .into()
        }
        _ => None,
    };
    if form
        .discussion_languages
        .as_deref()
        .is_some_and(|ids| ids.iter().any(|id| !matches!(id, 0 | 1)))
    {
        return Err(LemmyError::bad_request("couldnt_find_language"));
    }
    let mut updated = crate::profile::update_profile(
        &state,
        &current.account,
        crate::profile::ProfileChanges {
            display_name: form.display_name,
            note: form.bio,
            bot: form.bot_account,
            avatar,
            header: banner,
            ..crate::profile::ProfileChanges::default()
        },
    )
    .await
    .map_err(LemmyError::from)?;
    if clear_avatar {
        updated = crate::profile::clear_profile_image(
            &state,
            &updated,
            crate::profile::ProfileImage::Avatar,
        )
        .await
        .map_err(LemmyError::from)?;
    }
    if clear_banner {
        crate::profile::clear_profile_image(&state, &updated, crate::profile::ProfileImage::Header)
            .await
            .map_err(LemmyError::from)?;
    }
    if let Some(email) = form.email.as_deref() {
        user::update_email(
            &state.pool,
            current.user.id,
            (!email.trim().is_empty()).then_some(email),
        )
        .await?;
    }
    if form.interface_language.is_some() {
        user::update_locale(
            &state.pool,
            current.user.id,
            form.interface_language.as_deref(),
        )
        .await?;
    }
    let mut preferences = raw
        .as_object()
        .cloned()
        .ok_or_else(|| LemmyError::bad_request("invalid_form"))?;
    // These values are rendered from their native sources of truth. Keeping a
    // stale duplicate in the compatibility sidecar would let it override a
    // later native profile/e-mail change.
    for key in [
        "avatar",
        "banner",
        "display_name",
        "email",
        "bio",
        "bot_account",
        "discussion_languages",
        "interface_language",
    ] {
        preferences.remove(key);
    }
    plamenu_db::lemmy_user::merge_preferences(&state.pool, current.user.id, &preferences).await?;
    Ok(Json(json!({ "success": true })))
}
