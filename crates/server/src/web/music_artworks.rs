//! Artwork forms use the same music layout, CSRF checks and source revisions.
use super::{i18n::Locale, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_artworks::{self, Artwork, Target},
};
use axum::{
    Router,
    body::Bytes,
    extract::{Multipart, Path, State},
    response::Response,
    routing::post,
};
use maud::{Markup, html};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/web/music/{kind}/{id}/artworks/{slot}", post(upload))
        .route(
            "/web/music/{kind}/{id}/artworks/{slot}/remove",
            post(remove),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            music_artworks::MAX_BYTES + 256 * 1024,
        ))
}
#[must_use]
pub fn images(l: Locale, name: &str, items: &[Artwork]) -> Markup {
    html! {@for image in items {img.music-artwork.music-artwork--cover[image.slot=="cover"] src=(format!("/music/artwork/{}",image.id)) width=(image.width) height=(image.height) alt=(format!("{name}: {}",l.text(if image.slot=="cover" {"music-artwork-cover"} else {"music-artwork-image"})));}}
}
#[must_use]
pub fn forms(user: &WebUser, target: Target, id: i64, revision: i64, items: &[Artwork]) -> Markup {
    let l = user.locale;
    let slots: &[&str] = match target {
        Target::Assets => &["image"],
        Target::Artists | Target::Collections => &["image", "cover"],
        Target::Albums => &["cover"],
    };
    html! {details {summary{(l.text("music-artwork-edit"))}
        @for slot in slots {
            form.settings-form method="post" enctype="multipart/form-data" action=(format!("/web/music/{}/{id}/artworks/{slot}",target.path())){
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="revision" value=(revision);
                label for=(format!("music-artwork-{slot}")){(l.text(if *slot=="cover" {"music-artwork-cover-file"} else {"music-artwork-image-file"}))}
                input id=(format!("music-artwork-{slot}")) name="file" type="file" accept="image/jpeg,image/png,image/webp,image/gif,image/avif,image/jxl" required;
                p{(l.text("music-artwork-hint"))}
                button type="submit"{(l.text(if *slot=="cover" {"music-artwork-upload-cover"} else {"music-artwork-upload-image"}))}
            }
            @if items.iter().any(|item|item.slot==*slot){
                form.settings-form method="post" action=(format!("/web/music/{}/{id}/artworks/{slot}/remove",target.path())){
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="revision" value=(revision);
                    button.settings-button--danger type="submit"{(l.text(if *slot=="cover" {"music-artwork-remove-cover"} else {"music-artwork-remove-image"}))}
                }
            }
        }
    }}
}
async fn upload(
    State(state): State<AppState>,
    user: WebUser,
    Path((kind, id, slot)): Path<(String, i64, String)>,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let (bytes, revision, csrf) = crate::routes::music_artworks::form(multipart).await?;
    if csrf.as_deref() != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let target = Target::parse(&kind)?;
    music_artworks::upload(
        &state,
        user.current.account.id,
        target,
        id,
        &slot,
        &revision,
        bytes,
    )
    .await?;
    Ok(settings::redirect_to(&format!(
        "/music/{}/{id}",
        target.path()
    )))
}
async fn remove(
    State(state): State<AppState>,
    user: WebUser,
    Path((kind, id, slot)): Path<(String, i64, String)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let target = Target::parse(&kind)?;
    music_artworks::remove(
        &state,
        user.current.account.id,
        target,
        id,
        &slot,
        settings::field(&pairs, "revision").unwrap_or_default(),
    )
    .await?;
    Ok(settings::redirect_to(&format!(
        "/music/{}/{id}",
        target.path()
    )))
}
