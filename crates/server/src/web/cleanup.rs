//! The "Automated deletion" settings page — Mastodon's `/statuses_cleanup`
//! (web-only; there is no REST route for it upstream either). Saves the
//! account's [`statuses_cleanup`] policy; the server-side sweep worker does
//! the actual deleting.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::statuses_cleanup::{self, ALLOWED_MIN_STATUS_AGE, CleanupPolicy, PolicyUpdate};

use super::session::{WebUser, csrf_rejection};
use super::settings::{
    SettingsQuery, bad_form, checkbox, checked, field, form_pairs, redirect_to, saved_flash,
    select, settings_shell,
};
use crate::AppState;
use crate::error::ApiError;

/// The `ALLOWED_MIN_STATUS_AGE` ladder, as catalog identifiers — the ladder is
/// a fixed set of durations, so each gets its own message rather than a plural
/// rule (a language that inflects "week" differently at 1 and 2 needs both
/// forms spelled out anyway).
const AGE_LABELS: [&str; 8] = [
    "cleanup-age-1-week",
    "cleanup-age-2-weeks",
    "cleanup-age-1-month",
    "cleanup-age-2-months",
    "cleanup-age-3-months",
    "cleanup-age-6-months",
    "cleanup-age-1-year",
    "cleanup-age-2-years",
];

/// `GET /settings/statuses-cleanup` — the policy form.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let account_id = user.current.account.id;
    let policy = statuses_cleanup::get(&state.pool, account_id)
        .await?
        .unwrap_or_else(|| CleanupPolicy::defaults(account_id));

    let locale = user.locale;
    let ages: Vec<(String, String)> = ALLOWED_MIN_STATUS_AGE
        .iter()
        .zip(AGE_LABELS)
        .map(|(seconds, message)| (seconds.to_string(), locale.text(message)))
        .collect();
    let age_options: Vec<(&str, &str)> = ages
        .iter()
        .map(|(value, label)| (value.as_str(), label.as_str()))
        .collect();

    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("cleanup-saved")))
        form.settings-form method="post" action="/web/settings/statuses-cleanup" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("cleanup-legend")) }
                (checkbox("enabled", &locale.text("cleanup-enabled"),
                    &locale.text("cleanup-enabled-hint"), policy.enabled))
                label.settings-field {
                    span.settings-field__label { (locale.text("cleanup-age-label")) }
                    (select("min_status_age", &age_options,
                        &policy.min_status_age.to_string()))
                }
            }
            fieldset.settings-form__group {
                legend { (locale.text("cleanup-exceptions-legend")) }
                (checkbox("keep_pinned", &locale.text("cleanup-keep-pinned"),
                    &locale.text("cleanup-keep-pinned-hint"), policy.keep_pinned))
                (checkbox("keep_direct", &locale.text("cleanup-keep-direct"),
                    &locale.text("cleanup-keep-direct-hint"), policy.keep_direct))
                (checkbox("keep_self_fav", &locale.text("cleanup-keep-self-fav"),
                    &locale.text("cleanup-keep-self-fav-hint"), policy.keep_self_fav))
                (checkbox("keep_self_bookmark", &locale.text("cleanup-keep-self-bookmark"),
                    &locale.text("cleanup-keep-self-bookmark-hint"),
                    policy.keep_self_bookmark))
                (checkbox("keep_media", &locale.text("cleanup-keep-media"),
                    &locale.text("cleanup-keep-media-hint"), policy.keep_media))
                (checkbox("keep_polls", &locale.text("cleanup-keep-polls"),
                    &locale.text("cleanup-keep-polls-hint"), policy.keep_polls))
            }
            fieldset.settings-form__group {
                legend { (locale.text("cleanup-thresholds-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("cleanup-min-favs")) }
                    input type="number" name="min_favs" min="1" step="1"
                        value=[policy.min_favs]
                        placeholder=(locale.text("cleanup-no-limit"));
                    span.settings-field__hint { (locale.text("cleanup-min-favs-hint")) }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("cleanup-min-reblogs")) }
                    input type="number" name="min_reblogs" min="1" step="1"
                        value=[policy.min_reblogs]
                        placeholder=(locale.text("cleanup-no-limit"));
                    span.settings-field__hint { (locale.text("cleanup-min-reblogs-hint")) }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("cleanup-save")) }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/statuses-cleanup",
        &locale.text("cleanup-title"),
        &body,
    ))
}

/// An optional popularity threshold: empty = no limit, otherwise a count of
/// at least 1 (Mastodon's numericality validation).
fn parse_threshold(value: Option<&str>) -> Result<Option<i32>, ()> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<i32>() {
        Ok(count) if count >= 1 => Ok(Some(count)),
        _ => Err(()),
    }
}

/// `POST /web/settings/statuses-cleanup` — save the policy.
pub async fn save_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    // The select's values come from the fixed ladder, so anything else can
    // only be a tampered request (Mastodon validates inclusion the same way).
    let Some(min_status_age) = field(&pairs, "min_status_age")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|age| ALLOWED_MIN_STATUS_AGE.contains(age))
    else {
        return bad_form("invalid minimum age".into());
    };
    let (Ok(min_favs), Ok(min_reblogs)) = (
        parse_threshold(field(&pairs, "min_favs")),
        parse_threshold(field(&pairs, "min_reblogs")),
    ) else {
        return bad_form("thresholds must be at least 1".into());
    };
    let update = PolicyUpdate {
        enabled: checked(&pairs, "enabled"),
        min_status_age,
        keep_direct: checked(&pairs, "keep_direct"),
        keep_pinned: checked(&pairs, "keep_pinned"),
        keep_polls: checked(&pairs, "keep_polls"),
        keep_media: checked(&pairs, "keep_media"),
        keep_self_fav: checked(&pairs, "keep_self_fav"),
        keep_self_bookmark: checked(&pairs, "keep_self_bookmark"),
        min_favs,
        min_reblogs,
    };
    match statuses_cleanup::upsert(&state.pool, user.current.account.id, update).await {
        Ok(_) => redirect_to("/settings/statuses-cleanup?saved=1"),
        Err(err) => ApiError::from(err).into_response(),
    }
}
