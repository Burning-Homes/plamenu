//! Bounded creator library, separate from upload and review forms.
use super::{i18n::Locale, layout, session::WebUser, view};
use crate::{AppState, error::ApiError};
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    #[default]
    Published,
    Drafts,
    Live,
}
impl LibraryKind {
    fn value(&self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Drafts => "drafts",
            Self::Live => "live",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Published => "studio-published",
            Self::Drafts => "studio-drafts",
            Self::Live => "studio-live",
        }
    }
}
#[derive(Default, Deserialize)]
pub struct LibraryQuery {
    #[serde(default)]
    view: LibraryKind,
    q: Option<String>,
    before: Option<i64>,
}
#[derive(sqlx::FromRow)]
struct Entry {
    id: i64,
    media_id: i64,
    status_id: Option<i64>,
    title: String,
    channel: String,
    state: String,
    preview: Option<String>,
}

/// Native disclosure remains keyboard/touch usable without scripts. JS adds
/// hover preview, light dismissal and focus restoration to the same control.
pub(super) fn help(locale: Locale, title: &str, content: &Markup) -> Markup {
    html! { details.video-help data-video-help {
        summary aria-label=(format!("{}: {}",locale.text("studio-help"),locale.text(title))) {
            svg aria-hidden="true" viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="currentColor" stroke-width="1.5" {
                circle cx="12" cy="12" r="9" {}
                path d="M9.5 9a2.5 2.5 0 0 1 5 0c0 1.8-2.5 2-2.5 3.8" stroke-linecap="round" {}
                circle cx="12" cy="16" r="0.8" fill="currentColor" stroke="none" {}
            }
        }
        div.video-help__body {(content)}
    }}
}

/// Same human-readable picker as composer/settings; the native select submits the code.
pub(super) fn language_field(
    key: &str,
    selected: &str,
    required: bool,
    scope: &str,
    locale: Locale,
) -> Markup {
    let selected = (!selected.is_empty()).then_some(selected);
    html! { div.settings-field {
        span.settings-field__label {(locale.text(key))}
        (super::view::language_combo_optional_with_id("language", &locale.text(key),
            &crate::languages::enabled(None), selected,
            &locale.text(if required {"studio-language-choose"} else {"videos-unspecified"}),
            &format!("video-{scope}-language-options"), required))
    }}
}

pub(super) fn navigation(active: &str, locale: Locale) -> Markup {
    html! { nav.video-workspace__nav aria-label=(locale.text("studio-navigation")) {
        @for (href,key) in [("/videos","studio-library"),("/videos/channels","studio-channels"),("/videos/playlists?mine=true","studio-playlists"),("/videos/sources","studio-sources")] {
            a href=(href) aria-current=[(href==active).then_some("page")] {(locale.text(key))}
        }
    }}
}
pub(super) fn video_navigation(media_id: i64, status_id: i64, locale: Locale) -> Markup {
    html! { nav.video-workspace__nav aria-label=(locale.text("studio-navigation")) {
        a href=(format!("/videos/{media_id}/manage")) {(locale.text("studio-manage"))}
        a href=(format!("/web/statuses/{status_id}")) {(locale.text("studio-view"))}
    }}
}
pub(super) fn primary_actions(locale: Locale) -> Markup {
    html! { nav.video-workspace__actions aria-label=(locale.text("studio-create")) {
        a.video-workspace__primary href="/videos/upload" {(view::icon("upload")) (locale.text("studio-upload"))}
        a href="/videos/live/new" {(locale.text("videos-live-start"))}
        a href="/videos/channels/new" {(locale.text("videos-new-channel"))}
        details.video-workspace__more { summary {(locale.text("studio-more"))}
            a href="/videos/import" {(locale.text("video-import-title"))}
            a href="/videos/explore" {(locale.text("videos-explore-browse"))}
            a href="/videos/history" {(locale.text("video-history-title"))}
            a href="/videos/settings" {(locale.text("video-viewer-settings"))}
        }
    }}
}
fn private(response: Response) -> Response {
    super::video_history::private(response)
}

/// Cursor and search apply before LIMIT, and retain the same creator/editor
/// authority as existing publication management. No storage keys for drafts.
async fn entries(
    state: &AppState,
    actor: i64,
    query: &LibraryQuery,
) -> Result<Vec<Entry>, ApiError> {
    let sql=match query.view {
        LibraryKind::Published=>"SELECT v.status_id AS id,v.media_id, v.status_id, COALESCE(s.title,'Video') AS title,
            COALESCE(NULLIF(ca.display_name,''),ca.username) AS channel,s.visibility AS state,m.small_file_name AS preview
            FROM video_publications v JOIN video_channels ch ON ch.account_id=v.channel_id
            JOIN statuses s ON s.id=v.status_id JOIN media_attachments m ON m.id=v.media_id
            JOIN accounts ca ON ca.id=ch.account_id JOIN accounts o ON o.id=ch.owner_account_id
            WHERE ca.suspended_at IS NULL AND o.suspended_at IS NULL AND s.deleted_at IS NULL
              AND s.visibility IN ('public','unlisted') AND m.live_state IS NULL
              AND (ch.owner_account_id=$1 OR EXISTS(SELECT 1 FROM video_channel_grants g
                WHERE g.channel_id=ch.account_id AND g.account_id=$1 AND g.accepted AND (g.role='editor' OR s.account_id=$1)))
              AND ($2='' OR position(lower($2) in lower(COALESCE(s.title,'')))>0)
              AND ($3::bigint IS NULL OR v.status_id<$3) ORDER BY v.status_id DESC LIMIT 21\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos",
        LibraryKind::Drafts=>"SELECT d.media_id AS id,d.media_id,NULL::bigint AS status_id,d.title,
            COALESCE(NULLIF(a.display_name,''),a.username) AS channel,
            CASE WHEN d.expires_at<=now() THEN 'expired' ELSE d.state END AS state,NULL::text AS preview
            FROM video_drafts d JOIN media_attachments m ON m.id=d.media_id JOIN accounts a ON a.id=d.channel_id
            WHERE d.account_id=$1 AND m.account_id=$1 AND m.status_id IS NULL
              AND ($2='' OR position(lower($2) in lower(d.title))>0)
              AND ($3::bigint IS NULL OR d.media_id<$3) ORDER BY d.media_id DESC LIMIT 21",
        LibraryKind::Live=>"SELECT b.status_id AS id,b.media_id,b.status_id,COALESCE(s.title,'Video') AS title,
            COALESCE(NULLIF(a.display_name,''),a.username) AS channel,b.state,NULL::text AS preview
            FROM native_live_broadcasts b JOIN statuses s ON s.id=b.status_id JOIN accounts a ON a.id=b.channel_id
            WHERE b.owner_account_id=$1 AND s.deleted_at IS NULL
              AND ($2='' OR position(lower($2) in lower(COALESCE(s.title,'')))>0)
              AND ($3::bigint IS NULL OR b.status_id<$3) ORDER BY b.status_id DESC LIMIT 21\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos",
    };
    Ok(sqlx::query_as::<_, Entry>(sql)
        .bind(actor)
        .bind(query.q.as_deref().unwrap_or_default())
        .bind(query.before)
        .fetch_all(&state.pool)
        .await
        .map_err(plamenu_db::DbError::from)?)
}

pub async fn library(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<LibraryQuery>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:media")?;
    if query.q.as_ref().is_some_and(|s| s.chars().count() > 200)
        || query.before.is_some_and(|id| id <= 0)
    {
        return Err(ApiError::BadRequest("Invalid video library filter".into()));
    }
    let mut rows = entries(&state, user.current.account.id, &query).await?;
    let more = rows.len() > 20;
    rows.truncate(20);
    let locale = user.locale;
    let body = html! { section.column.video-workspace {
        header.video-workspace__header { h1 {(locale.text("studio-title"))} p {(locale.text("studio-intro"))} }
        (primary_actions(locale))
        (navigation("/videos",locale))
        form.video-workspace__search method="get" action="/videos" {
            input type="hidden" name="view" value=(query.view.value());
            label { span.visually-hidden {(locale.text("studio-search"))} input type="search" name="q" maxlength="200" placeholder=(locale.text("studio-search")) value=(query.q.as_deref().unwrap_or_default()); }
            button type="submit" {(locale.text("studio-search-submit"))}
        }
        nav.video-workspace__tabs aria-label=(locale.text("studio-content")) {
            @for kind in [LibraryKind::Published,LibraryKind::Drafts,LibraryKind::Live] {
                @let params=serde_urlencoded::to_string([("view",kind.value()),("q",query.q.as_deref().unwrap_or_default())]).unwrap_or_default();
                a href=(format!("/videos?{params}")) aria-current=[(kind.value()==query.view.value()).then_some("page")] {(locale.text(kind.label()))}
            }
        }
        h2.visually-hidden {(locale.text(query.view.label()))}
        @if rows.is_empty() {div.video-workspace__empty {p {(locale.text("studio-empty"))} a href="/videos/upload" {(locale.text("studio-upload"))}}}
        ul.video-workspace__library {
            @for entry in &rows {
                @let target=match query.view {LibraryKind::Published=>format!("/videos/{}/manage",entry.media_id),LibraryKind::Drafts=>format!("/videos/drafts/{}",entry.media_id),LibraryKind::Live=>format!("/videos/live/{}",entry.id)};
                li.video-workspace__row {
                    a.video-workspace__thumbnail href=(target) tabindex="-1" aria-hidden="true" {
                        @if let Some(preview)=&entry.preview {img src=(format!("/media/{preview}")) alt="" loading="lazy" width="128" height="72";}
                        @else {span {"▶"}}
                    }
                    div.video-workspace__item {
                        a.video-workspace__title href=(target) {(entry.title)}
                        p.video-workspace__meta {(entry.channel) " · " (locale.text(&format!("studio-state-{}",entry.state)))}
                    }
                    @if let Some(status)=entry.status_id {
                        details.status__menu data-status-menu {
                            summary.action aria-label=(locale.text("status-more-options")) title=(locale.text("status-more-options")) {(view::icon("more"))}
                            div.status__menu-pop {
                                a.status__menu-item href=(format!("/web/statuses/{status}")) {(locale.text("studio-view"))}
                                a.status__menu-item href=(target) {(locale.text("studio-manage"))}
                            }
                        }
                    }
                }
            }
        }
        @if more && let Some(last)=rows.last() {
            @let params=serde_urlencoded::to_string([("view",query.view.value().to_owned()),("q",query.q.clone().unwrap_or_default()),("before",last.id.to_string())]).unwrap_or_default();
            a.pager__more href=(format!("/videos?{params}")) {(locale.text("studio-older"))}
        }
    }};
    Ok(private(
        layout::shell(&locale.text("studio-title"), Some(&user), &body).into_response(),
    ))
}

pub async fn manage(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:media")?;
    let item =
        plamenu_db::video_captions::owned_publication(&state.pool, id, user.current.account.id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let editable = crate::video_edit::ready_native_ids(&state, &[id])
        .await?
        .contains(&id);
    let locale = user.locale;
    let body = html! { section.column.video-workspace {
        p {a href="/videos" {(locale.text("studio-library"))}}
        header.video-workspace__header {h1 {(item.title)} a href=(format!("/web/statuses/{}",item.status_id)) {(locale.text("studio-view"))}}
        nav.video-workspace__tools aria-label=(locale.text("studio-manage")) {
            @for (path,key,hint) in [(format!("/web/statuses/{}/edit",item.status_id),"status-edit","studio-details-hint"),
                (format!("/videos/{id}/metadata"),"videos-metadata-manage","studio-poster-hint"),
                (format!("/videos/{id}/captions"),"videos-caption-published-title","studio-captions-hint"),
                (format!("/videos/{id}/chapters"),"videos-chapters-title","studio-chapters-hint"),
                (format!("/videos/{id}/sharing"),"video-sharing-title","studio-sharing-hint")] {
                a href=(path) {strong {(locale.text(key))} span {(locale.text(hint))}}
            }
            @if editable {a href=(format!("/videos/{id}/edit")) {strong {(locale.text("video-edit-title"))} span {(locale.text("studio-edit-hint"))}}}
        }
    }};
    Ok(private(
        layout::shell(&item.title, Some(&user), &body).into_response(),
    ))
}
