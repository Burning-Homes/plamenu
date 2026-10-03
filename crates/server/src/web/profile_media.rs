//! Account profile uploads shared by delegated group/channel editors.
use super::i18n::Locale;
use axum::{
    extract::Multipart,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::account::Account;

#[derive(Default)]
pub(super) struct Form {
    pub pairs: Vec<(String, String)>,
    pub avatar: Option<Vec<u8>>,
    pub header: Option<Vec<u8>>,
}

pub(super) async fn read(multipart: &mut Multipart) -> Result<Form, Response> {
    let mut form = Form::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid form: {e}")).into_response())?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if matches!(name.as_str(), "avatar" | "header") {
            let bytes = field.bytes().await.map_err(|e| {
                (StatusCode::BAD_REQUEST, format!("upload failed: {e}")).into_response()
            })?;
            if bytes.len() > crate::media_processing::MAX_UPLOAD_BYTES {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response());
            }
            if !bytes.is_empty() {
                if name == "avatar" {
                    form.avatar = Some(bytes.to_vec());
                } else {
                    form.header = Some(bytes.to_vec());
                }
            }
        } else {
            form.pairs
                .push((name, field.text().await.unwrap_or_default()));
        }
    }
    Ok(form)
}

pub(super) fn fields(account: &Account, locale: Locale) -> Markup {
    html! {
        @for (name, description, label, hint, alt_label, value) in [
            ("avatar","avatar_description","settings-profile-avatar","settings-profile-avatar-hint","settings-profile-avatar-description",&account.avatar_description),
            ("header","header_description","settings-profile-header","settings-profile-header-hint","settings-profile-header-description",&account.header_description),
        ] {
            label.settings-field {
                span.settings-field__label {(locale.text(label))}
                input type="file" name=(name) accept="image/*";
                span.settings-field__hint {(locale.text(hint))}
            }
            label.settings-field {
                span.settings-field__label {(locale.text(alt_label))}
                input name=(description) value=(value) maxlength="1500";
                span.settings-field__hint {(locale.text("settings-profile-image-description-hint"))}
            }
        }
    }
}
