//! Live user settings for the first-party web UI.
//!
//! The pages mirror the Mastodon settings surface Plamenu supports today:
//! profile identity, posting/reading preferences, privacy flags, and basic
//! account credentials. Unsupported features are called out directly instead
//! of rendered as disabled controls.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Multipart, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::DbError;
use plamenu_db::account::Account;
use plamenu_db::notification_policy::{self, Disposition};
use plamenu_db::user::{
    self, DefaultQuotePolicy, PostingDefaultFormat, PostingDefaultVisibility, ReadingExpandMedia,
    ThreadOrder, TimelineOrder, UserSettings,
};
use serde::Deserialize;
use serde_json::Value;

use super::i18n::Locale;
use super::layout;
use super::session::{self, WebUser, csrf_rejection};
use super::view::{self, icon};
use crate::error::ApiError;
use crate::media_processing::MAX_UPLOAD_BYTES;
use crate::profile::{ProfileChanges, update_profile};
use crate::routes::accounts_api::apply_text_param;
use crate::state::AppState;
use crate::{languages, time_zones};

/// Mastodon's profile limits, surfaced as `maxlength` hints on the inputs.
const MAX_DISPLAY_NAME_CHARS: usize = 30;
const MAX_NOTE_CHARS: usize = 500;
const MAX_FIELDS: usize = 4;
const MAX_FIELD_CHARS: usize = 255;

struct Section {
    href: &'static str,
    message: &'static str,
}

const SECTIONS: &[Section] = &[
    Section {
        href: "/settings/profile",
        message: "settings-section-profile",
    },
    Section {
        href: "/settings/preferences",
        message: "settings-section-preferences",
    },
    Section {
        href: "/settings/languages",
        message: "settings-section-languages",
    },
    Section {
        href: "/settings/privacy",
        message: "settings-section-privacy",
    },
    Section {
        href: "/settings/push",
        message: "settings-section-push",
    },
    Section {
        href: "/settings/filters",
        message: "settings-section-filters",
    },
    Section {
        href: "/settings/custom-emojis",
        message: "settings-section-custom-emojis",
    },
    Section {
        href: "/settings/scheduled",
        message: "settings-section-scheduled",
    },
    Section {
        href: "/settings/featured-tags",
        message: "settings-section-featured-tags",
    },
    Section {
        href: "/settings/collections",
        message: "settings-section-collections",
    },
    Section {
        href: "/settings/relationships",
        message: "settings-section-relationships",
    },
    Section {
        href: "/settings/account",
        message: "settings-section-account",
    },
    Section {
        href: "/settings/aliases",
        message: "settings-section-aliases",
    },
    Section {
        href: "/settings/migration",
        message: "settings-section-migration",
    },
    Section {
        href: "/settings/security",
        message: "settings-section-security",
    },
    Section {
        href: "/settings/strikes",
        message: "settings-section-strikes",
    },
    Section {
        href: "/settings/export",
        message: "settings-section-export",
    },
    Section {
        href: "/settings/statuses-cleanup",
        message: "settings-section-statuses-cleanup",
    },
    Section {
        href: "/settings/applications",
        message: "settings-section-applications",
    },
    Section {
        href: "/settings/invites",
        message: "settings-section-invites",
    },
    Section {
        href: "/settings/identity-proofs",
        message: "identity-title",
    },
];

#[derive(Deserialize)]
pub struct SettingsQuery {
    pub(super) saved: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) fn settings_shell(user: &WebUser, current: &str, title: &str, body: &Markup) -> Markup {
    let locale = user.locale;
    let tab_data = SECTIONS
        .iter()
        // The Invites section only exists for roles granting
        // `invite_users`; its endpoints 403 without it.
        .filter(|section| section.href != "/settings/invites" || user.can_invite)
        .map(|section| {
            (
                section.href,
                locale.text(section.message),
                section.href == current,
            )
        })
        .collect::<Vec<_>>();
    let tabs = tab_data
        .iter()
        .map(|(href, label, active)| view::Tab::new(href, label, *active))
        .collect::<Vec<_>>();
    let content = html! {
        section.column.settings {
            header.settings__head {
                h1 { (icon("settings")) " " (locale.text("nav-settings")) }
            }
            (view::tab_strip(
                &locale.text("settings-sections"),
                &tabs,
            ))
            div.settings__body {
                h2.settings__title { (title) }
                (body)
            }
        }
    };
    layout::shell(title, Some(user), &content)
}

pub(super) fn saved_flash(show: bool, message: &str) -> Markup {
    html! {
        @if show {
            p.settings__saved role="status" { (message) }
        }
    }
}

pub(super) fn error_flash(message: Option<&str>) -> Markup {
    html! {
        @if let Some(message) = message {
            p.settings__error id="form-error" role="alert" { (message) }
        }
    }
}

pub(super) fn redirect_to(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path.to_owned())]).into_response()
}

pub(super) fn form_pairs(body: &Bytes) -> Result<Vec<(String, String)>, String> {
    serde_urlencoded::from_bytes(body).map_err(|e| format!("invalid form body: {e}"))
}

pub(super) fn bad_form(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

pub(super) fn field<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| !matches!(v, "" | "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"))
}

pub(super) fn checked(pairs: &[(String, String)], name: &str) -> bool {
    truthy(field(pairs, name))
}

/// `GET /settings` - redirect to the first section.
pub async fn index() -> Redirect {
    Redirect::to("/settings/profile")
}

// ---- Profile -----------------------------------------------------------

/// The stored `(name, value, verified)` metadata fields, padded to `MAX_FIELDS`
/// empty rows so the form always offers every slot. `verified` reflects the
/// rel="me" check stamped by the verification worker.
fn field_rows(account: &Account) -> Vec<(String, String, bool)> {
    let mut rows: Vec<(String, String, bool)> = account
        .fields
        .as_array()
        .map(|stored| {
            stored
                .iter()
                .map(|field| {
                    let get = |key: &str| {
                        field
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    let verified = field.get("verified_at").and_then(Value::as_str).is_some();
                    (get("name"), get("value"), verified)
                })
                .collect()
        })
        .unwrap_or_default();
    rows.truncate(MAX_FIELDS);
    rows.resize(MAX_FIELDS, (String::new(), String::new(), false));
    rows
}

pub async fn profile_form(user: WebUser, Query(query): Query<SettingsQuery>) -> Markup {
    let account = &user.current.account;
    let fields = field_rows(account);
    let locale = user.locale;
    let mut bio_hint_args = FluentArgs::new();
    bio_hint_args.set(
        "count",
        i32::try_from(MAX_NOTE_CHARS).expect("profile bio limit fits in i32"),
    );
    let mut metadata_hint_args = FluentArgs::new();
    metadata_hint_args.set(
        "count",
        i32::try_from(MAX_FIELDS).expect("profile field limit fits in i32"),
    );

    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("settings-profile-saved")))
        form.settings-form method="post" action="/web/settings/profile" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(user.csrf);

            fieldset.settings-form__group {
                legend { (locale.text("settings-profile-appearance")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("settings-profile-display-name")) }
                    input type="text" name="display_name" maxlength=(MAX_DISPLAY_NAME_CHARS)
                        value=(account.display_name) autocomplete="nickname";
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("settings-profile-bio")) }
                    textarea name="note" rows="4" maxlength=(MAX_NOTE_CHARS) { (account.note_source) }
                    span.settings-field__hint {
                        (locale.text_with("settings-profile-bio-hint", &bio_hint_args))
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("settings-profile-avatar")) }
                    input type="file" name="avatar" accept="image/*";
                    span.settings-field__hint { (locale.text("settings-profile-avatar-hint")) }
                }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-profile-avatar-description"))
                    }
                    input type="text" name="avatar_description" maxlength="1500"
                        value=(account.avatar_description);
                    span.settings-field__hint {
                        (locale.text("settings-profile-image-description-hint"))
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("settings-profile-header")) }
                    input type="file" name="header" accept="image/*";
                    span.settings-field__hint { (locale.text("settings-profile-header-hint")) }
                }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-profile-header-description"))
                    }
                    input type="text" name="header_description" maxlength="1500"
                        value=(account.header_description);
                    span.settings-field__hint {
                        (locale.text("settings-profile-image-description-hint"))
                    }
                }
            }

            fieldset.settings-form__group {
                legend { (locale.text("settings-profile-metadata")) }
                p.settings-field__hint {
                    (locale.text_with("settings-profile-metadata-hint", &metadata_hint_args))
                }
                @for (index, (name, value, verified)) in fields.iter().enumerate() {
                    div.settings-field__pair {
                        label.settings-field {
                            span.settings-field__label {
                                (locale.text("settings-profile-field-label"))
                            }
                            input type="text" name=(format!("fields_attributes[{index}][name]"))
                                maxlength=(MAX_FIELD_CHARS) value=(name);
                        }
                        label.settings-field {
                            span.settings-field__label {
                                (locale.text("settings-profile-field-content"))
                                @if *verified {
                                    " " span.settings-field__verified {
                                        (icon("check")) " "
                                        (locale.text("settings-profile-field-verified"))
                                    }
                                }
                            }
                            input type="text" name=(format!("fields_attributes[{index}][value]"))
                                maxlength=(MAX_FIELD_CHARS) value=(value);
                        }
                    }
                }
            }

            fieldset.settings-form__group {
                legend { (locale.text("settings-profile-behavior")) }
                (checkbox(
                    "locked",
                    &locale.text("settings-profile-locked"),
                    &locale.text("settings-profile-locked-hint"),
                    account.locked,
                ))
                (checkbox(
                    "bot",
                    &locale.text("settings-profile-bot"),
                    &locale.text("settings-profile-bot-hint"),
                    account.is_bot,
                ))
            }

            div.settings-form__actions {
                button type="submit" { (locale.text("settings-profile-save")) }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/profile",
        &locale.text("settings-profile-title"),
        &body,
    )
}

pub(super) fn checkbox(name: &str, label: &str, hint: &str, is_checked: bool) -> Markup {
    html! {
        label.settings-toggle {
            input type="hidden" name=(name) value="false";
            input type="checkbox" name=(name) value="true" checked[is_checked];
            span.settings-toggle__text {
                span.settings-toggle__label { (label) }
                span.settings-field__hint { (hint) }
            }
        }
    }
}

pub(super) fn select(name: &str, options: &[(&str, &str)], selected: &str) -> Markup {
    html! {
        select name=(name) {
            @for (value, label) in options {
                option value=(value) selected[*value == selected] { (label) }
            }
        }
    }
}

pub async fn update_profile_action(
    State(state): State<AppState>,
    user: WebUser,
    mut multipart: Multipart,
) -> Response {
    let mut changes = ProfileChanges::default();
    let mut fields: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut csrf = String::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("invalid form: {e}")).into_response();
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "avatar" | "header" => {
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return (StatusCode::BAD_REQUEST, format!("upload failed: {e}"))
                            .into_response();
                    }
                };
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response();
                }
                if !bytes.is_empty() {
                    if name == "avatar" {
                        changes.avatar = Some(bytes.to_vec());
                    } else {
                        changes.header = Some(bytes.to_vec());
                    }
                }
            }
            _ => {
                let text = field.text().await.unwrap_or_default();
                apply_text_param(&mut changes, &mut fields, &name, text);
            }
        }
    }

    if !user.csrf_ok(&csrf) {
        return csrf_rejection();
    }
    if !fields.is_empty() {
        changes.fields = Some(fields.into_values().collect());
    }

    match update_profile(&state, &user.current.account, changes).await {
        Ok(_) => redirect_to("/settings/profile?saved=1"),
        Err(err) => err.into_response(),
    }
}

// ---- Preferences -------------------------------------------------------

/// The stored posting-visibility default as its form/select value.
fn visibility_value(v: PostingDefaultVisibility) -> &'static str {
    match v {
        PostingDefaultVisibility::Default => "default",
        PostingDefaultVisibility::Public => "public",
        PostingDefaultVisibility::Unlisted => "unlisted",
        PostingDefaultVisibility::Private => "private",
        PostingDefaultVisibility::Direct => "direct",
        PostingDefaultVisibility::Local => "local",
    }
}

/// The time-zone select: "server default" plus every known IANA
/// zone, ordered west to east by current UTC offset and labelled with the
/// exact delta ("(UTC+02:00) Europe/Berlin") so the choice is unambiguous.
fn time_zone_field(current: &str, locale: Locale) -> Markup {
    let labelled = crate::web::clock::zone_options();
    let server_default = locale.text("settings-preferences-time-zone-default");
    let mut zone_options: Vec<(&str, &str)> = vec![("", &server_default)];
    zone_options.extend(labelled.iter().map(|(name, label)| (*name, label.as_str())));
    html! {
        label.settings-field {
            span.settings-field__label {
                (locale.text("settings-preferences-time-zone"))
            }
            (select("time_zone", &zone_options, current))
            span.settings-field__hint {
                (locale.text("settings-preferences-time-zone-hint"))
            }
        }
    }
}

/// The push-alert checkbox inventory: every notification kind with a
/// push rendering (`crate::web_push`'s `title`), its label, and whether a
/// fresh subscription starts with it on. Edit notifications default off —
/// they are the noisy ones.
const PUSH_ALERTS: &[(&str, &str, bool)] = &[
    ("mention", "settings-push-mention", true),
    ("follow", "settings-push-follow", true),
    ("follow_request", "settings-push-follow-request", true),
    ("favourite", "settings-push-favourite", true),
    ("reblog", "settings-push-boost", true),
    ("quote", "settings-push-quote", true),
    (
        "pleroma:emoji_reaction",
        "settings-push-emoji-reaction",
        true,
    ),
    ("poll", "settings-push-poll", true),
    ("status", "settings-push-status", true),
    ("update", "settings-push-update", false),
    ("quoted_update", "settings-push-quoted-update", false),
    ("live", "settings-push-live", true),
    // Event participation (E-track). An event you are attending changing under
    // you is the one of these you cannot afford to miss, so all default on.
    (
        "event.participation",
        "settings-push-event-participation",
        true,
    ),
    ("event.accepted", "settings-push-event-accepted", true),
    ("event.rejected", "settings-push-event-rejected", true),
    ("event.changed", "settings-push-event-changed", true),
    ("event.invite", "settings-push-event-invite", true),
];

/// `GET /settings/push` — Web Push for this browser. Inherently
/// JavaScript-driven (a service worker receives the pushes), so this page is
/// the one deliberate exception to the no-JS-first rule: the server renders
/// inert scaffolding plus the VAPID-key hook, `app.js` reveals and drives the
/// controls, and a no-JS visitor reads the explanation instead of finding
/// broken buttons. Subscriptions ride the session-cookie-authenticated
/// `/api/web/push_subscriptions` endpoints, keyed per browser.
pub async fn push_notifications(
    State(state): State<AppState>,
    user: WebUser,
) -> Result<Markup, ApiError> {
    let locale = user.locale;
    let vapid = crate::web_push::vapid(&state)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let body = html! {
        p.settings-field__hint {
            (locale.text("settings-push-intro"))
        }
        p data-push-nojs {
            (locale.text("settings-push-nojs"))
        }
        div data-push data-push-key=(vapid.public_key)
            data-push-unsupported=(locale.text("settings-push-unsupported"))
            data-push-server-error=(locale.text("settings-push-server-error"))
            data-push-timeout=(locale.text("settings-push-timeout"))
            data-push-permission-blocked=(locale.text("settings-push-permission-blocked"))
            data-push-contacting=(locale.text("settings-push-contacting"))
            data-push-enabled=(locale.text("settings-push-enabled"))
            data-push-enable-failed=(locale.text("settings-push-enable-failed"))
            data-push-saved=(locale.text("settings-push-saved"))
            data-push-save-failed=(locale.text("settings-push-save-failed"))
            data-push-disabled=(locale.text("settings-push-disabled"))
            data-push-disable-failed=(locale.text("settings-push-disable-failed"))
            hidden {
            p.settings__saved data-push-status role="status" hidden {}
            div.settings-form__actions {
                button type="button" data-push-enable hidden {
                    (locale.text("settings-push-enable"))
                }
            }
            form.settings-form data-push-alerts hidden {
                fieldset.settings-form__group {
                    legend { (locale.text("settings-push-notify-about")) }
                    @for (kind, message, default_on) in PUSH_ALERTS {
                        (checkbox(kind, &locale.text(message), "", *default_on))
                    }
                }
                div.settings-form__actions {
                    button type="button" data-push-disable {
                        (locale.text("settings-push-disable"))
                    }
                }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/push",
        &locale.text("settings-push-title"),
        &body,
    ))
}

#[allow(clippy::too_many_lines)]
pub async fn preferences(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let interface_locale = user.locale;
    let settings = user::settings_by_user_id(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default();
    let time_zone = user::time_zone(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default();
    let visibility_value = visibility_value(settings.posting_default_visibility);
    let visibility_options = [
        (
            "default",
            interface_locale.text("settings-preferences-visibility-automatic"),
        ),
        (
            "public",
            interface_locale.text("settings-preferences-visibility-public"),
        ),
        (
            "unlisted",
            interface_locale.text("settings-preferences-visibility-unlisted"),
        ),
        (
            "private",
            interface_locale.text("settings-preferences-visibility-followers"),
        ),
        (
            "direct",
            interface_locale.text("settings-preferences-visibility-mentioned"),
        ),
        (
            "local",
            interface_locale.text("settings-preferences-visibility-local"),
        ),
    ];
    let visibility_options = visibility_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let quote_options = [
        (
            "public",
            interface_locale.text("settings-preferences-quote-anyone"),
        ),
        (
            "followers",
            interface_locale.text("settings-preferences-quote-followers"),
        ),
        (
            "nobody",
            interface_locale.text("settings-preferences-quote-only-you"),
        ),
    ];
    let quote_options = quote_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let format_options = [
        (
            "text/plain",
            interface_locale.text("settings-preferences-format-plain"),
        ),
        (
            "text/markdown",
            interface_locale.text("settings-preferences-format-markdown"),
        ),
        (
            "text/html",
            interface_locale.text("settings-preferences-format-html"),
        ),
    ];
    let format_options = format_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let timeline_options = [
        (
            "published",
            interface_locale.text("settings-preferences-timeline-published"),
        ),
        (
            "received",
            interface_locale.text("settings-preferences-timeline-received"),
        ),
    ];
    let timeline_options = timeline_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let thread_options = [
        (
            "tree",
            interface_locale.text("settings-preferences-thread-tree"),
        ),
        (
            "flat",
            interface_locale.text("settings-preferences-thread-flat"),
        ),
    ];
    let thread_options = thread_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let media_options = [
        (
            "default",
            interface_locale.text("settings-preferences-media-default"),
        ),
        (
            "show_all",
            interface_locale.text("settings-preferences-media-show"),
        ),
        (
            "hide_all",
            interface_locale.text("settings-preferences-media-hide"),
        ),
    ];
    let media_options = media_options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let body = html! {
        (saved_flash(
            query.saved.is_some(),
            &interface_locale.text("settings-preferences-saved"),
        ))
        form.settings-form method="post" action="/web/settings/preferences" data-settings-deps {
            input type="hidden" name="csrf" value=(user.csrf);

            fieldset.settings-form__group {
                legend { (interface_locale.text("settings-preferences-posting")) }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-default-privacy"))
                    }
                    (select("posting_default_visibility", &visibility_options, visibility_value))
                }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-quote-policy"))
                    }
                    (select("posting_default_quote_policy", &quote_options,
                        settings.posting_default_quote_policy.as_str()))
                }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-default-format"))
                    }
                    (select("posting_default_content_type", &format_options,
                        settings.posting_default_content_type.as_str()))
                    span.settings-field__hint {
                        (interface_locale.text("settings-preferences-default-format-hint"))
                    }
                }
                (checkbox("posting_default_sensitive",
                    &interface_locale.text("settings-preferences-sensitive"),
                    &interface_locale.text("settings-preferences-sensitive-hint"),
                    settings.posting_default_sensitive))
                (checkbox("show_application",
                    &interface_locale.text("settings-preferences-show-application"),
                    &interface_locale.text("settings-preferences-show-application-hint"),
                    settings.show_application))
            }

            fieldset.settings-form__group {
                legend { (interface_locale.text("settings-preferences-reading")) }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-timeline-order"))
                    }
                    (select("timeline_order", &timeline_options, settings.timeline_order.as_str()))
                }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-thread-order"))
                    }
                    (select("thread_order", &thread_options, settings.thread_order.as_str()))
                    span.settings-field__hint {
                        (interface_locale.text("settings-preferences-thread-order-hint"))
                    }
                }
                label.settings-field {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-media-display"))
                    }
                    (select("reading_expand_media", &media_options,
                        settings.reading_expand_media.as_str()))
                }
                (checkbox("reading_expand_spoilers",
                    &interface_locale.text("settings-preferences-expand-spoilers"),
                    &interface_locale.text("settings-preferences-expand-spoilers-hint"),
                    settings.reading_expand_spoilers))
                (checkbox("reading_autoplay_gifs",
                    &interface_locale.text("settings-preferences-autoplay-gifs"),
                    &interface_locale.text("settings-preferences-autoplay-gifs-hint"),
                    settings.reading_autoplay_gifs))
                (checkbox("reading_allow_direct_remote_media",
                    &interface_locale.text("settings-preferences-direct-media"),
                    &interface_locale.text("settings-preferences-direct-media-hint"),
                    settings.reading_allow_direct_remote_media))
                (checkbox("reading_collapse_boosts",
                    &interface_locale.text("settings-preferences-collapse-boosts"),
                    &interface_locale.text("settings-preferences-collapse-boosts-hint"),
                    settings.reading_collapse_boosts))
                (time_zone_field(&time_zone, interface_locale))
            }

            fieldset.settings-form__group {
                legend { (interface_locale.text("settings-preferences-notifications")) }
                (checkbox("live_notifications",
                    &interface_locale.text("settings-preferences-live-notifications"),
                    &interface_locale.text("settings-preferences-live-notifications-hint"),
                    user.live_notifications))
                div data-show-when="live_notifications" {
                    (checkbox("notification_sound",
                        &interface_locale.text("settings-preferences-notification-sound"),
                        &interface_locale.text("settings-preferences-notification-sound-hint"),
                        user.notification_sound))
                }
                label.settings-field data-show-when="notification_sound" {
                    span.settings-field__label {
                        (interface_locale.text("settings-preferences-notification-volume"))
                    }
                    input type="range" name="notification_volume" min="0" max="100" step="5"
                        value=(user.notification_volume);
                    span.settings-field__hint {
                        (interface_locale.text("settings-preferences-notification-volume-hint"))
                    }
                }
            }

            div.settings-form__actions {
                button type="submit" {
                    (interface_locale.text("settings-preferences-save"))
                }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/preferences",
        &interface_locale.text("settings-preferences-title"),
        &body,
    ))
}

pub async fn update_preferences_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }

    // `noindex` lives on the privacy page and the language fields on the
    // Languages page; carry the stored values so saving preferences doesn't
    // reset them.
    let stored = match user::settings_by_user_id(&state.pool, user.current.user.id).await {
        Ok(stored) => stored.unwrap_or_default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let settings = UserSettings {
        posting_default_visibility: PostingDefaultVisibility::parse(
            field(&pairs, "posting_default_visibility").unwrap_or("default"),
        ),
        posting_default_sensitive: checked(&pairs, "posting_default_sensitive"),
        posting_default_language: stored.posting_default_language.clone(),
        posting_default_quote_policy: DefaultQuotePolicy::parse(
            field(&pairs, "posting_default_quote_policy").unwrap_or("public"),
        ),
        posting_default_content_type: PostingDefaultFormat::parse(
            field(&pairs, "posting_default_content_type").unwrap_or("text/plain"),
        ),
        reading_expand_media: ReadingExpandMedia::parse(
            field(&pairs, "reading_expand_media").unwrap_or("default"),
        ),
        reading_expand_spoilers: checked(&pairs, "reading_expand_spoilers"),
        reading_autoplay_gifs: checked(&pairs, "reading_autoplay_gifs"),
        reading_allow_direct_remote_media: checked(&pairs, "reading_allow_direct_remote_media"),
        reading_collapse_boosts: checked(&pairs, "reading_collapse_boosts"),
        reading_translate_language: stored.reading_translate_language.clone(),
        timeline_order: TimelineOrder::parse(field(&pairs, "timeline_order").unwrap_or_default()),
        thread_order: ThreadOrder::parse(field(&pairs, "thread_order").unwrap_or_default()),
        noindex: stored.noindex,
        show_application: checked(&pairs, "show_application"),
        time_zone: stored.time_zone.clone(),
    };

    // Time zone stores separately from `UserSettings` (a standalone column).
    // The form is a select over the inventory; an unknown value is a tampered
    // request, so refuse rather than clearing silently.
    let time_zone = field(&pairs, "time_zone").unwrap_or("").trim();
    let normalized = if time_zone.is_empty() {
        None
    } else {
        match time_zones::normalize(time_zone) {
            Some(zone) => Some(zone),
            None => return bad_form(format!("unknown time zone: {time_zone}")),
        }
    };
    if let Err(err) = user::update_time_zone(&state.pool, user.current.user.id, normalized).await {
        return ApiError::from(err).into_response();
    }

    let notification_preferences = plamenu_db::web_setting::NotificationPreferences {
        live_updates: checked(&pairs, "live_notifications"),
        sound: checked(&pairs, "notification_sound"),
        volume: field(&pairs, "notification_volume")
            .and_then(|value| value.parse::<u8>().ok())
            .map_or(20, |volume| volume.min(100)),
    };

    match user::update_settings(&state.pool, user.current.user.id, settings).await {
        Ok(Some(_)) => {
            if let Err(err) = plamenu_db::web_setting::update_notification_preferences(
                &state.pool,
                user.current.user.id,
                notification_preferences,
            )
            .await
            {
                return ApiError::from(err).into_response();
            }
            redirect_to("/settings/preferences?saved=1")
        }
        Ok(None) => ApiError::NotFound.into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Languages ----------------------------------------------------------

/// The interface languages the picker offers: [`Locale::AVAILABLE`] resolved
/// against the posting-language inventory so they render as proper names.
fn interface_languages() -> Vec<&'static languages::Language> {
    Locale::AVAILABLE
        .iter()
        .filter_map(|code| languages::find(code))
        .collect()
}

/// One of the two language checklists. The inventory is ~200 rows, so each
/// list lives behind a `<details>` disclosure — folded, the page reads as
/// three short language settings instead of two walls of checkboxes. The
/// summary carries the current count so the selection is legible without
/// opening it.
fn language_checklist(locale: Locale, name: &str, chosen: &[String]) -> Markup {
    let count = i64::try_from(chosen.len()).unwrap_or(i64::MAX);
    let summary = if count == 0 {
        locale.text("settings-languages-all-chosen")
    } else {
        let mut args = FluentArgs::new();
        args.set("count", count);
        locale.text_with("settings-languages-selected", &args)
    };
    html! {
        details.settings-langs__disclosure {
            summary.settings-langs__summary {
                span.settings-langs__summary-text {
                    (locale.text("settings-languages-choose"))
                }
                span.settings-langs__count { (summary) }
                span.settings-langs__caret { (icon("chevron")) }
            }
            div.settings-langs__panel {
                // JS-only conveniences over the plain checkboxes; the no-JS
                // form works without them.
                div.settings-langs__controls hidden data-lang-controls {
                    button type="button" data-lang-select-all {
                        (locale.text("settings-languages-select-all"))
                    }
                    button type="button" data-lang-select-none {
                        (locale.text("settings-languages-select-none"))
                    }
                }
                div.settings-langs {
                    @for language in languages::LANGUAGES {
                        label.settings-langs__item {
                            input type="checkbox" name=(name) value=(language.code)
                                checked[chosen.iter().any(|code| code == language.code)];
                            span { (language.label()) }
                        }
                    }
                }
            }
        }
    }
}

/// `GET /settings/languages` — every language preference in one place: the
/// interface language, the posting default plus the set the composer offers,
/// and the reading filter plus the translate-into target.
pub async fn languages_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let locale = user.locale;
    let settings = user::settings_by_user_id(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default();
    // The stored interface locale is seeded from `Accept-Language` at sign-in
    // and may carry a code we have no catalog for; the combo renders such a
    // value verbatim rather than silently switching the user to English.
    let interface = user::locale(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_else(|| "en".to_owned());
    let enabled = user::posting_languages(&state.pool, user.current.user.id).await?;
    let chosen = enabled.unwrap_or_default();
    let reading = user::chosen_languages(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default();
    let body = html! {
        (saved_flash(
            query.saved.is_some(),
            &locale.text("settings-languages-saved"),
        ))
        form.settings-form method="post" action="/web/settings/languages" {
            input type="hidden" name="csrf" value=(user.csrf);

            fieldset.settings-form__group {
                legend { (locale.text("settings-preferences-interface")) }
                // A div, not a label: the combo carries its own label and
                // nesting labels is invalid HTML.
                div.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-preferences-interface-language"))
                    }
                    (view::language_combo(
                        "locale",
                        &locale.text("settings-preferences-interface-language"),
                        &interface_languages(),
                        &interface,
                        locale,
                    ))
                    span.settings-field__hint {
                        (locale.text("settings-preferences-interface-language-hint"))
                    }
                }
            }

            fieldset.settings-form__group {
                legend { (locale.text("settings-preferences-posting")) }
                div.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-preferences-default-language"))
                    }
                    (view::language_combo(
                        "posting_default_language",
                        &locale.text("settings-preferences-default-language"),
                        &languages::enabled(None),
                        &settings.posting_default_language,
                        locale,
                    ))
                    span.settings-field__hint {
                        (locale.text("settings-preferences-default-language-hint"))
                    }
                }
                div.settings-field {
                    span.settings-field__label { (locale.text("settings-languages-posting")) }
                    span.settings-field__hint {
                        (locale.text("settings-languages-posting-hint"))
                    }
                    (language_checklist(locale, "languages[]", &chosen))
                }
            }

            fieldset.settings-form__group {
                legend { (locale.text("settings-preferences-reading")) }
                div.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-preferences-translate-into"))
                    }
                    (view::language_combo_optional(
                        "reading_translate_language",
                        &locale.text("settings-preferences-translate-into"),
                        &languages::enabled(None),
                        settings.reading_translate_language.as_deref(),
                        &locale.text("settings-preferences-translate-default"),
                    ))
                    span.settings-field__hint {
                        (locale.text("settings-preferences-translate-hint"))
                    }
                }
                div.settings-field {
                    span.settings-field__label { (locale.text("settings-languages-reading")) }
                    span.settings-field__hint {
                        (locale.text("settings-languages-reading-hint"))
                    }
                    (language_checklist(locale, "reading_languages[]", &reading))
                }
            }

            div.settings-form__actions {
                button type="submit" { (locale.text("settings-languages-save")) }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/languages",
        &locale.text("settings-languages-title"),
        &body,
    ))
}

/// `POST /web/settings/languages` — store the interface locale, the two
/// single-language preferences, and the two checked sets. Checking nothing in
/// a set clears that restriction.
pub async fn update_languages_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let mut chosen: Vec<String> = Vec::new();
    let mut reading: Vec<String> = Vec::new();
    for (key, value) in &pairs {
        let target = match key.as_str() {
            "languages[]" | "languages" => &mut chosen,
            "reading_languages[]" | "reading_languages" => &mut reading,
            _ => continue,
        };
        let code = value.trim();
        // Checkbox values come from the inventory, so an unknown code can
        // only be a tampered request.
        if languages::find(code).is_none() {
            return bad_form(format!("unknown language: {code}"));
        }
        if !target.iter().any(|existing| existing == code) {
            target.push(code.to_owned());
        }
    }

    // The three single-value pickers are selects over a known set, so an
    // unknown code can only be a tampered request — refuse it rather than
    // storing a garbage default. A field the request omits entirely keeps its
    // stored value; only an explicitly empty translate-into means "follow the
    // posting language", which stores as NULL.
    let posting_default_language = field(&pairs, "posting_default_language").map(str::trim);
    if let Some(code) = posting_default_language
        && languages::find(code).is_none()
    {
        return bad_form(format!("unknown language: {code}"));
    }
    let reading_translate_language = field(&pairs, "reading_translate_language").map(str::trim);
    if let Some(code) = reading_translate_language
        && !code.is_empty()
        && languages::find(code).is_none()
    {
        return bad_form(format!("unknown language: {code}"));
    }

    // The interface picker only offers locales with a catalog, so anything
    // else is a tampered request — except the value already stored, which was
    // seeded from `Accept-Language` and must echo back unchanged rather than
    // brick the form. Locale stores separately from `UserSettings`.
    if let Some(interface) = field(&pairs, "locale").map(str::trim) {
        if !Locale::AVAILABLE.contains(&interface) {
            let stored_locale = match user::locale(&state.pool, user.current.user.id).await {
                Ok(stored_locale) => stored_locale,
                Err(err) => return ApiError::from(err).into_response(),
            };
            if stored_locale.as_deref() != Some(interface) {
                return bad_form(format!("unknown interface language: {interface}"));
            }
        }
        if let Err(err) =
            user::update_locale(&state.pool, user.current.user.id, Some(interface)).await
        {
            return ApiError::from(err).into_response();
        }
    }

    // The rest of `UserSettings` lives on the preferences page; carry the
    // stored values so saving languages doesn't reset them.
    let mut settings = match user::settings_by_user_id(&state.pool, user.current.user.id).await {
        Ok(stored) => stored.unwrap_or_default(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Some(code) = posting_default_language {
        settings.posting_default_language = code.to_owned();
    }
    if let Some(code) = reading_translate_language {
        settings.reading_translate_language = (!code.is_empty()).then(|| code.to_owned());
    }
    match user::update_settings(&state.pool, user.current.user.id, settings).await {
        Ok(Some(_)) => {}
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    }

    if let Err(err) =
        user::update_chosen_languages(&state.pool, user.current.user.id, Some(&reading)).await
    {
        return ApiError::from(err).into_response();
    }
    match user::update_posting_languages(&state.pool, user.current.user.id, Some(&chosen)).await {
        Ok(true) => redirect_to("/settings/languages?saved=1"),
        Ok(false) => ApiError::NotFound.into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Privacy and reach -------------------------------------------------

/// One notification-policy category: the shared accept/filter/drop select.
fn policy_row(locale: Locale, name: &str, label: &str, current: Disposition) -> Markup {
    let options = [
        ("accept", locale.text("settings-privacy-policy-accept")),
        ("filter", locale.text("settings-privacy-policy-filter")),
        ("drop", locale.text("settings-privacy-policy-drop")),
    ];
    let options = options
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    html! {
        label.settings-field {
            span.settings-field__label { (label) }
            (select(name, &options, current.as_str()))
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn privacy(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let locale = user.locale;
    let account = &user.current.account;
    let policy = notification_policy::get_or_default(&state.pool, account.id).await?;
    let settings = user::settings_by_user_id(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default();
    let attribution_domains =
        plamenu_db::account::attribution_domains(&state.pool, account.id).await?;
    let body = html! {
        (saved_flash(
            query.saved.is_some(),
            &locale.text("settings-privacy-saved"),
        ))
        form.settings-form method="post" action="/web/settings/privacy" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("settings-privacy-discoverability")) }
                (checkbox("discoverable",
                    &locale.text("settings-privacy-discoverable"),
                    &locale.text("settings-privacy-discoverable-hint"),
                    account.discoverable.unwrap_or(false)))
                (checkbox("indexable",
                    &locale.text("settings-privacy-indexable"),
                    &locale.text("settings-privacy-indexable-hint"),
                    account.indexable))
                (checkbox("hide_collections",
                    &locale.text("settings-privacy-hide-collections"),
                    &locale.text("settings-privacy-hide-collections-hint"),
                    account.hide_collections))
                (checkbox("noindex",
                    &locale.text("settings-privacy-noindex"),
                    &locale.text("settings-privacy-noindex-hint"),
                    settings.noindex))
            }
            fieldset.settings-form__group {
                legend { (locale.text("settings-privacy-profile-tabs")) }
                (checkbox("show_featured",
                    &locale.text("settings-privacy-show-featured"),
                    &locale.text("settings-privacy-show-featured-hint"),
                    account.show_featured))
                (checkbox("show_media",
                    &locale.text("settings-privacy-show-media"),
                    &locale.text("settings-privacy-show-media-hint"),
                    account.show_media))
                (checkbox("show_media_replies",
                    &locale.text("settings-privacy-show-media-replies"),
                    &locale.text("settings-privacy-show-media-replies-hint"),
                    account.show_media_replies))
            }
            fieldset.settings-form__group {
                legend { (locale.text("settings-privacy-attribution")) }
                label.settings-field {
                    span.settings-field__label {
                        (locale.text("settings-privacy-attribution-domains"))
                    }
                    textarea name="attribution_domains" rows="3" placeholder="example.com" {
                        (attribution_domains.join("\n"))
                    }
                    span.settings-field__hint {
                        (locale.text("settings-privacy-attribution-hint"))
                    }
                }
            }
            fieldset.settings-form__group {
                legend { (locale.text("settings-privacy-notification-filtering")) }
                p.settings-field__hint {
                    (locale.text("settings-privacy-notification-prefix"))
                    " "
                    a href="/notifications/requests" {
                        (locale.text("settings-privacy-filtered-notifications"))
                    }
                    " "
                    (locale.text("settings-privacy-notification-suffix"))
                }
                (policy_row(locale, "not_following_policy",
                    &locale.text("settings-privacy-not-following"),
                    policy.for_not_following))
                (policy_row(locale, "not_followers_policy",
                    &locale.text("settings-privacy-not-followers"),
                    policy.for_not_followers))
                (policy_row(locale, "new_accounts_policy",
                    &locale.text("settings-privacy-new-accounts"),
                    policy.for_new_accounts))
                (policy_row(locale, "private_mentions_policy",
                    &locale.text("settings-privacy-private-mentions"),
                    policy.for_private_mentions))
                (policy_row(locale, "limited_accounts_policy",
                    &locale.text("settings-privacy-limited-accounts"),
                    policy.for_limited_accounts))
                (policy_row(locale, "bots_policy",
                    &locale.text("settings-privacy-bots"),
                    policy.for_bots))
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("settings-privacy-save")) }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/privacy",
        &locale.text("settings-privacy-title"),
        &body,
    ))
}

pub async fn update_privacy_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    // The textarea is newline-separated, like Mastodon's verification page.
    let attribution_domains = field(&pairs, "attribution_domains").map(|raw| {
        crate::profile::normalize_attribution_domains(
            &raw.lines().map(str::to_owned).collect::<Vec<_>>(),
        )
    });
    let changes = ProfileChanges {
        discoverable: Some(checked(&pairs, "discoverable")),
        indexable: Some(checked(&pairs, "indexable")),
        hide_collections: Some(checked(&pairs, "hide_collections")),
        show_media: Some(checked(&pairs, "show_media")),
        show_media_replies: Some(checked(&pairs, "show_media_replies")),
        show_featured: Some(checked(&pairs, "show_featured")),
        attribution_domains,
        ..ProfileChanges::default()
    };
    match update_profile(&state, &user.current.account, changes).await {
        Ok(_) => {
            let mut settings =
                match user::settings_by_user_id(&state.pool, user.current.user.id).await {
                    Ok(settings) => settings.unwrap_or_default(),
                    Err(err) => return ApiError::from(err).into_response(),
                };
            settings.noindex = checked(&pairs, "noindex");
            if let Err(err) =
                user::update_settings(&state.pool, user.current.user.id, settings).await
            {
                return ApiError::from(err).into_response();
            }
            let mut policy =
                match notification_policy::get_or_default(&state.pool, user.current.account.id)
                    .await
                {
                    Ok(policy) => policy,
                    Err(err) => return ApiError::from(err).into_response(),
                };
            for (name, slot) in [
                ("not_following_policy", &mut policy.for_not_following),
                ("not_followers_policy", &mut policy.for_not_followers),
                ("new_accounts_policy", &mut policy.for_new_accounts),
                ("private_mentions_policy", &mut policy.for_private_mentions),
                ("limited_accounts_policy", &mut policy.for_limited_accounts),
                ("bots_policy", &mut policy.for_bots),
            ] {
                if let Some(value) = field(&pairs, name) {
                    *slot = Disposition::parse(value);
                }
            }
            if let Err(err) =
                notification_policy::upsert(&state.pool, user.current.account.id, policy).await
            {
                return ApiError::from(err).into_response();
            }
            redirect_to("/settings/privacy?saved=1")
        }
        Err(err) => err.into_response(),
    }
}

// ---- Account -----------------------------------------------------------

fn account_error(locale: Locale, code: Option<&str>) -> Option<String> {
    match code {
        Some("current_password" | "email_password" | "delete_password") => {
            Some(locale.text("settings-account-error-password"))
        }
        Some("email_taken") => Some(locale.text("settings-account-error-email-taken")),
        Some("delete_confirm") => Some(locale.text("settings-account-error-delete-confirm")),
        _ => None,
    }
}

fn account_saved(locale: Locale, code: Option<&str>) -> Option<String> {
    match code {
        Some("email") => Some(locale.text("settings-account-email-updated")),
        Some("email_removed") => Some(locale.text("settings-account-email-removed")),
        _ => None,
    }
}

pub async fn account(user: WebUser, Query(query): Query<SettingsQuery>) -> Markup {
    let locale = user.locale;
    let handle = format!("@{}", user.current.account.username);
    let saved = account_saved(locale, query.saved.as_deref());
    let error = account_error(locale, query.error.as_deref());
    let error_code = query.error.as_deref();
    let email_invalid = error_code == Some("email_taken");
    let email_password_invalid = matches!(error_code, Some("email_password" | "current_password"));
    let delete_handle_invalid = error_code == Some("delete_confirm");
    let delete_password_invalid =
        matches!(error_code, Some("delete_password" | "current_password"));
    let mut delete_args = FluentArgs::new();
    delete_args.set("handle", handle.as_str());
    let body = html! {
        (saved_flash(saved.is_some(), saved.as_deref().unwrap_or_default()))
        (error_flash(error.as_deref()))

        div.settings-form {
            form.settings-form method="post" action="/web/settings/account/email" {
                input type="hidden" name="csrf" value=(user.csrf);
                fieldset.settings-form__group {
                    legend { (locale.text("settings-account-email")) }
                    label.settings-field {
                        span.settings-field__label {
                            (locale.text("settings-account-email-address"))
                        }
                        input type="email" name="email"
                            value=(user.current.user.email.as_deref().unwrap_or(""))
                            autocomplete="email"
                            aria-invalid=[email_invalid.then_some("true")]
                            aria-describedby=(if email_invalid {
                                "settings-email-hint form-error"
                            } else {
                                "settings-email-hint"
                            });
                        span.settings-field__hint id="settings-email-hint" {
                            (locale.text("settings-account-email-hint"))
                        }
                    }
                    label.settings-field {
                        span.settings-field__label {
                            (locale.text("settings-account-current-password"))
                        }
                        input type="password" name="current_password"
                            autocomplete="current-password" required
                            aria-invalid=[email_password_invalid.then_some("true")]
                            aria-describedby=[email_password_invalid.then_some("form-error")];
                    }
                }
                div.settings-form__actions {
                    button type="submit" { (locale.text("settings-account-change-email")) }
                }
            }

            form.settings-form method="post" action="/web/settings/account/delete" {
                input type="hidden" name="csrf" value=(user.csrf);
                fieldset.settings-form__group.settings-form__group--danger {
                    legend { (locale.text("settings-account-danger-zone")) }
                    p.settings-field__hint id="settings-delete-hint" {
                        (locale.text_with("settings-account-delete-hint", &delete_args))
                    }
                    label.settings-field {
                        span.settings-field__label {
                            (locale.text("settings-account-type-handle"))
                        }
                        input type="text" name="confirm_handle" autocomplete="off"
                            placeholder=(handle) required
                            aria-invalid=[delete_handle_invalid.then_some("true")]
                            aria-describedby=(if delete_handle_invalid {
                                "settings-delete-hint form-error"
                            } else {
                                "settings-delete-hint"
                            });
                    }
                    label.settings-field {
                        span.settings-field__label {
                            (locale.text("settings-account-current-password"))
                        }
                        input type="password" name="current_password"
                            autocomplete="current-password" required
                            aria-invalid=[delete_password_invalid.then_some("true")]
                            aria-describedby=[delete_password_invalid.then_some("form-error")];
                    }
                }
                div.settings-form__actions {
                    button.settings-button--danger type="submit" {
                        (locale.text("settings-account-delete"))
                    }
                }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/account",
        &locale.text("settings-account-title"),
        &body,
    )
}

pub async fn update_email_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if !crate::auth::verify_password_gated(
        field(&pairs, "current_password")
            .unwrap_or_default()
            .to_owned(),
        user.current.user.password_hash.clone(),
    )
    .await
    {
        return redirect_to("/settings/account?error=email_password");
    }
    // Empty removes the address: e-mail is optional, so an account may go
    // back to signing in by username only.
    let email = field(&pairs, "email").unwrap_or_default().trim();
    let saved = if email.is_empty() {
        "email_removed"
    } else {
        "email"
    };
    let email = (!email.is_empty()).then_some(email);
    match user::update_email(&state.pool, user.current.user.id, email).await {
        Ok(Some(_)) => redirect_to(&format!("/settings/account?saved={saved}")),
        Ok(None) => ApiError::NotFound.into_response(),
        Err(DbError::EmailTaken) => redirect_to("/settings/account?error=email_taken"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

pub async fn delete_account_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if !crate::auth::verify_password_gated(
        field(&pairs, "current_password")
            .unwrap_or_default()
            .to_owned(),
        user.current.user.password_hash.clone(),
    )
    .await
    {
        return redirect_to("/settings/account?error=delete_password");
    }
    let expected = format!("@{}", user.current.account.username);
    if field(&pairs, "confirm_handle").unwrap_or_default().trim() != expected {
        return redirect_to("/settings/account?error=delete_confirm");
    }

    // Mastodon's DeleteAccountService: suspend into a tombstone, federate
    // Delete(Actor), purge content, destroy the login.
    if let Err(err) = crate::moderation::self_delete_account(&state, &user.current.account).await {
        return err.into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/".to_owned()),
            (header::SET_COOKIE, session::clear_session_cookie()),
        ],
    )
        .into_response()
}
