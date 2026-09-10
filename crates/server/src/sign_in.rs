//! Side effects shared by the API (`oauth`) and web login flows: recording a
//! successful sign-in and the optional new-IP alert e-mail.

use plamenu_db::user::{self, SignInContext};
use plamenu_db::{DbError, login_activity};

use crate::AppState;

/// Records a successful sign-in and, when the account opted in and mail is
/// configured, e-mails a notification if the source IP has never been used
/// before. The alert is best-effort — a failure never blocks the login.
pub async fn record(
    state: &AppState,
    user_id: i64,
    locale: Option<&str>,
    ip: Option<&str>,
    ctx: SignInContext<'_>,
) -> Result<(), DbError> {
    // Familiarity is checked before the record inserts this login's own row, so
    // the current IP isn't counted as already-seen.
    let new_ip = match ip {
        Some(ip) => !login_activity::is_familiar_ip(&state.pool, user_id, ip)
            .await
            .unwrap_or(true),
        None => false,
    };
    let user_agent = ctx.user_agent.map(str::to_owned);
    user::record_sign_in_with_ip(&state.pool, user_id, locale, ip, ctx).await?;
    if new_ip && let Err(error) = alert_new_ip(state, user_id, ip, user_agent.as_deref()).await {
        tracing::warn!(error = %error.chain(), user = user_id, "new-ip sign-in alert failed");
    }
    Ok(())
}

/// Sends the new-IP notification when the account has the alert on, an address
/// on file, and the server can send mail. All three off by default.
async fn alert_new_ip(
    state: &AppState,
    user_id: i64,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<(), crate::error::ApiError> {
    if !crate::mailer::enabled(state) {
        return Ok(());
    }
    let Some(prefs) = user::new_ip_alert_prefs(&state.pool, user_id).await? else {
        return Ok(());
    };
    if !prefs.enabled {
        return Ok(());
    }
    let Some(email) = prefs.email else {
        return Ok(());
    };
    let domain = &state.config.domain;
    // The mail is read by the account owner, so it is written in their stored
    // interface locale, never the sign-in request's `Accept-Language`.
    let locale = crate::web::i18n::Locale::for_user(&state.pool, user_id).await?;
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("domain", domain.as_str());
    args.set(
        "ip",
        ip.map_or_else(
            || locale.plain("email-signin-unknown-address"),
            str::to_owned,
        ),
    );
    args.set(
        "agent",
        user_agent.map_or_else(
            || locale.plain("email-signin-unknown-device"),
            str::to_owned,
        ),
    );
    args.set("url", format!("https://{domain}/settings/security"));
    let subject = locale.plain_with("email-signin-subject", &args);
    let body = format!(
        "{intro}\n\n{ip}\n{device}\n\n{outro}\n",
        intro = locale.plain("email-signin-intro"),
        ip = locale.plain_with("email-signin-ip", &args),
        device = locale.plain_with("email-signin-device", &args),
        outro = locale.plain_with("email-signin-outro", &args),
    );
    crate::mailer::enqueue(state, &email, &subject, &body).await
}
