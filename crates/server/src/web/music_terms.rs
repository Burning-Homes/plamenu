//! Exact genre/tag discovery belongs to the native Music workspace.
use super::{layout, session::MaybeWebUser};
use crate::{
    AppState,
    error::ApiError,
    music_feeds::{self, Term},
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
async fn show(
    state: AppState,
    kind: Term,
    name: String,
    user: Option<super::session::WebUser>,
    headers: HeaderMap,
    before: Option<String>,
) -> Result<Response, ApiError> {
    if let Some(user) = &user {
        user.current.require_scope("read:statuses")?;
    }
    let viewer = user.as_ref().map(|user| user.current.account.id);
    let name = music_feeds::term_name(&name)?;
    let page = music_feeds::term_page(&state, kind, &name, viewer, before.as_deref()).await?;
    let l = user.as_ref().map_or_else(
        || super::i18n::Locale::from_headers(&headers),
        |user| user.locale,
    );
    let encoded = music_feeds::term_segment(&name);
    let base = format!("/music/feeds/{}/{encoded}", kind.path());
    let body = html! {section.column.music-library{h1{(name)}(super::music::workspace_navigation(l,user.is_some(),"public"))
        p{a href=(format!("{base}.rss")){(l.text("music-feed-rss"))} " · " a href=(format!("{base}.atom")){(l.text("music-feed-atom"))}}
        @if page.items.is_empty(){p{(l.text("music-empty"))}}
        ul{@for item in &page.items{li{a href=(item.link){(item.title)}}}}
        @if let Some(before)=page.before {a rel="next" href=(format!("/music/{}/{encoded}?before={before}",kind.path())){(l.text("music-older"))}}
    }};
    let meta = layout::PageMeta {
        feed_url: Some(format!("{base}.atom")),
        rss_feed_url: Some(format!("{base}.rss")),
        ..Default::default()
    };
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell_with_metadata(&name, user.as_ref(), &body, &meta, l),
    )
        .into_response())
}

#[derive(Default, serde::Deserialize)]
pub struct Page {
    before: Option<String>,
}
pub async fn tags(
    State(state): State<AppState>,
    Path(name): Path<String>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    show(state, Term::Tag, name, user, headers, page.before).await
}
pub async fn genres(
    State(state): State<AppState>,
    Path(name): Path<String>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    show(state, Term::Genre, name, user, headers, page.before).await
}
