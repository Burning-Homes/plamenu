//! Ordinary HTML distribution choices, before or after publication.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let result=async{
        user.current.require_scope("read:media")?;
        let (policy,publication)=crate::video_distribution::owned(&state,user.current.account.id,id).await?;
        let back=publication.map_or_else(||format!("/videos/drafts/{id}/review"),|p|format!("/web/statuses/{}",p.status_id));
        let locale=user.locale;
        let body=html!{
            section.column.video-sharing{
                h1{(locale.text("video-sharing-title"))}
                p{(locale.text("video-sharing-hint"))}
                @if query.get("saved").is_some_and(|s|s=="1"){p.settings__saved role="status"{(locale.text("video-sharing-saved"))}}
                p{a href=(back){(locale.text("video-sharing-back"))}}
                form.settings-form method="post" action=(format!("/web/videos/{id}/sharing")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    label.settings-field__choice{input type="checkbox" name="download_enabled" value="yes" checked[policy.download_enabled];span{(locale.text("video-sharing-download"))}}
                    label.settings-field__choice{input type="checkbox" name="embed_enabled" value="yes" checked[policy.embed_enabled];span{(locale.text("video-sharing-embed"))}}
                    button type="submit"{(locale.text("video-sharing-save"))}
                }
            }
        };
        Ok::<_,ApiError>(layout::shell(&locale.text("video-sharing-title"),Some(&user),&body).into_response())
    }.await;
    private(result.unwrap_or_else(IntoResponse::into_response))
}
pub async fn save(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let result = async {
        let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
        if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
            return Ok(csrf_rejection());
        }
        user.current.require_scope("write:media")?;
        crate::video_distribution::save(
            &state,
            user.current.account.id,
            plamenu_db::video_distribution::Policy {
                media_id: id,
                download_enabled: field(&pairs, "download_enabled") == Some("yes"),
                embed_enabled: field(&pairs, "embed_enabled") == Some("yes"),
            },
            user.current.require_scope("write:statuses").is_ok(),
        )
        .await?;
        Ok::<_, ApiError>(redirect_to(&format!("/videos/{id}/sharing?saved=1")))
    }
    .await;
    private(result.unwrap_or_else(IntoResponse::into_response))
}
