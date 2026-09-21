//! Server-rendered review step for destructive web forms.
//!
//! JavaScript enhances these forms with a compact `window.confirm` prompt.
//! Without JavaScript they POST here first, preserving every submitted field,
//! and only the explicit second submit reaches the destructive endpoint.

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use maud::html;

use super::actions::safe_return;
use super::layout;
use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, field, form_pairs};

pub async fn review(user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let action = field(&pairs, "confirm_action").unwrap_or_default();
    let message = field(&pairs, "confirm_message").unwrap_or_default();
    if !valid_action(action) || message.trim().is_empty() {
        return bad_form("invalid confirmation request".to_owned());
    }
    let locale = user.locale;
    let back = safe_return(field(&pairs, "return_to"), "/");
    let title = locale.text("confirmation-title");
    let content = review_content(locale, action, message, &back, &pairs);
    layout::shell(&title, Some(&user), &content).into_response()
}

fn review_content(
    locale: super::i18n::Locale,
    action: &str,
    message: &str,
    back: &str,
    pairs: &[(String, String)],
) -> maud::Markup {
    html! {
        section.auth-card.confirmation-card {
            h1 { (locale.text("confirmation-title")) }
            p { (locale.text("confirmation-lead")) }
            p.confirmation-card__message { (message) }
            form.settings-form method="post" action=(action) {
                @for (name, value) in pairs {
                    @if name != "confirm_action" && name != "confirm_message" {
                        input type="hidden" name=(name) value=(value);
                    }
                }
                div.settings-form__actions {
                    button.settings-button--danger type="submit" {
                        (locale.text("confirmation-continue"))
                    }
                    a.settings-button--plain href=(back) {
                        (locale.text("confirmation-cancel"))
                    }
                }
            }
        }
    }
}

fn valid_action(action: &str) -> bool {
    action.starts_with("/web/") && !action.starts_with("//") && action != "/web/confirm"
}

#[cfg(test)]
mod tests {
    use super::{review_content, valid_action};
    use crate::web::i18n::Locale;

    #[test]
    fn confirmation_targets_only_state_changing_web_routes() {
        assert!(valid_action("/web/statuses/1/delete"));
        assert!(valid_action("/web/settings/lists/1/delete"));
        assert!(!valid_action("/web/confirm"));
        assert!(!valid_action("//attacker.test/web/delete"));
        assert!(!valid_action("https://attacker.test/web/delete"));
        assert!(!valid_action("/settings/account"));
    }

    #[test]
    fn review_preserves_the_original_submission_but_not_gateway_metadata() {
        let pairs = vec![
            ("csrf".to_owned(), "test-token".to_owned()),
            ("return_to".to_owned(), "/settings/filters/7".to_owned()),
            (
                "confirm_action".to_owned(),
                "/web/filters/7/delete".to_owned(),
            ),
            (
                "confirm_message".to_owned(),
                "Delete this filter?".to_owned(),
            ),
            ("keyword".to_owned(), "one".to_owned()),
            ("keyword".to_owned(), "two".to_owned()),
        ];
        let rendered = review_content(
            Locale::default(),
            "/web/filters/7/delete",
            "Delete this filter?",
            "/settings/filters/7",
            &pairs,
        )
        .into_string();

        assert!(rendered.contains(r#"action="/web/filters/7/delete""#));
        assert!(rendered.contains("Delete this filter?"));
        assert!(rendered.contains(r#"href="/settings/filters/7""#));
        assert_eq!(rendered.matches(r#"name="keyword""#).count(), 2);
        assert!(!rendered.contains(r#"name="confirm_action""#));
        assert!(!rendered.contains(r#"name="confirm_message""#));
    }
}
