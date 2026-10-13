//! Native one-time links, sharing management and anonymous capability previews.
use super::{
    i18n::Locale,
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_saved::{Kind, Reference},
    music_shares::{self, New},
};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
#[derive(Default, serde::Deserialize)]
pub struct Page {
    before: Option<String>,
    #[serde(default)]
    include_revoked: bool,
}
#[derive(Default, serde::Deserialize)]
pub struct Preview {
    #[serde(default)]
    token: String,
    max_id: Option<i64>,
}
fn response(title: &str, user: Option<&WebUser>, l: Locale, body: &Markup) -> Response {
    let mut response =
        layout::shell_visitor_localized(title, user, layout::AnonNav::default(), body, l)
            .into_response();
    for (key, value) in [
        (header::CACHE_CONTROL, "private, no-store"),
        (header::VARY, "Cookie"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        response.headers_mut().insert(key, value.parse().unwrap());
    }
    response
        .headers_mut()
        .insert("x-robots-tag", "noindex, noarchive".parse().unwrap());
    response
}
fn name(source: &serde_json::Value) -> &str {
    source["name"]
        .as_str()
        .or_else(|| source["title"].as_str())
        .unwrap_or_default()
}
pub async fn manager(
    State(state): State<AppState>,
    user: WebUser,
    Query(input): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let page = music_shares::page(
        &state,
        user.current.account.id,
        input.before.as_deref(),
        input.include_revoked,
    )
    .await?;
    let l = user.locale;
    let title = l.text("music-shares-title");
    let clock = super::pages::clock_for(Some(&user), l);
    let body = html! {section.column.music-library{
        h1{(title)}(super::music::workspace_navigation(l,true,"shares"))p{(l.text("music-shares-hint"))}p{a href="/music"{(l.text("music-title"))} " · " a href="/music/mine"{(l.text("music-mine-title"))}}
        form.settings-form method="get" action="/music/shares"{label{input type="checkbox" name="include_revoked" value="true" checked[input.include_revoked];" "(l.text("music-shares-show-revoked"))}button type="submit"{(l.text("music-search-submit"))}}
        @if page.items.is_empty(){p{(l.text("music-shares-empty"))}}
        ul{@for item in &page.items{li{
            @if let Some(source)=&item.source{@if let Some(url)=source["url"].as_str(){a href=(url){(name(source))}}}@else{span{(l.text("music-saved-unavailable"))}}
            p{(l.text(if item.category=="url"{"music-shares-link"}else{"music-shares-member"}))
                @if let Some(username)=&item.username{" — "(username)}
                @if item.revoked{" — "(l.text("music-shares-revoked"))}@else if item.expired{" — "(l.text("music-shares-expired"))}
            }
            @if let Some(expires)=&item.expires_at{p{(l.text("music-shares-expires"))": " (clock.element_absolute_iso(expires))}}
            @if !item.revoked{form method="post" action="/web/music/shares/revoke"{
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="category" value=(item.category);
                input type="hidden" name="link_id" value=(item.id);input type="hidden" name="kind" value=(item.reference.kind.text());input type="hidden" name="id" value=(item.reference.id);
                @if let Some(member)=&item.member_id{input type="hidden" name="member" value=(member);}
                button type="submit"{(l.text("music-assets-revoke"))}
            }}
        }}}
        @if let Some(before)=page.before{a href=(format!("/music/shares?include_revoked={}&before={before}",input.include_revoked)){(l.text("music-older"))}}
    }};
    Ok(response(&title, Some(&user), l, &body))
}
pub async fn new_link(
    State(state): State<AppState>,
    user: WebUser,
    Query(reference): Query<Reference>,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let (reference, source) =
        music_shares::management_source(&state, user.current.account.id, &reference).await?;
    let l = user.locale;
    let title = l.text("music-shares-create");
    let body = html! {section.column.music-library{h1{(title)}h2{(name(&source))}p{(l.text("music-shares-create-hint"))}
        form.settings-form method="post" action="/web/music/share-links"{
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="kind" value=(reference.kind.text());input type="hidden" name="id" value=(reference.id);
            label for="share-expiration"{(l.text("music-shares-expires"))}select id="share-expiration" name="expires"{
                @for(value,label)in [("day","music-shares-day"),("week","music-shares-week"),("month","music-shares-month"),("never","music-shares-never")]{option value=(value) selected[value=="week"]{(l.text(label))}}
            }
            button type="submit"{(l.text("music-shares-create"))}
        }p{a href="/music/shares"{(l.text("music-shares-title"))}}
    }};
    Ok(response(&title, Some(&user), l, &body))
}
fn pairs(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:media")?;
    let fields = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(fields)
}
fn reference(fields: &[(String, String)]) -> Result<Reference, ApiError> {
    Ok(Reference {
        kind: Kind::parse(settings::field(fields, "kind").unwrap_or_default())?,
        id: settings::field(fields, "id").unwrap_or_default().to_owned(),
    })
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = pairs(&user, &body)?;
    let duration = match settings::field(&fields, "expires") {
        Some("day") => Some(time::Duration::days(1)),
        Some("week") => Some(time::Duration::weeks(1)),
        Some("month") => Some(time::Duration::days(30)),
        Some("never") => None,
        _ => return Err(ApiError::Unprocessable("Choose a link expiration".into())),
    };
    let expires_at = duration
        .map(|v| crate::entities::rfc3339(time::OffsetDateTime::now_utc() + v))
        .transpose()?;
    let link = music_shares::create(
        &state,
        user.current.account.id,
        &New {
            reference: reference(&fields)?,
            expires_at,
        },
    )
    .await?;
    let l = user.locale;
    let title = l.text("music-shares-created");
    let body = html! {section.column.music-library{h1{(title)}p{(l.text("music-shares-once"))}label for="music-share-url"{(l.text("music-shares-link"))}input id="music-share-url" type="url" readonly value=(link.url);p{a href=(link.url) rel="noreferrer"{(l.text("music-shares-preview"))}}p{a href="/music/shares"{(l.text("music-shares-title"))}}}};
    Ok(response(&title, Some(&user), l, &body))
}
pub async fn revoke(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = pairs(&user, &body)?;
    if settings::field(&fields, "category") == Some("url") {
        let id = settings::field(&fields, "link_id")
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(ApiError::NotFound)?;
        music_shares::revoke(&state, user.current.account.id, id).await?;
    } else if settings::field(&fields, "category") == Some("grant") {
        let member = settings::field(&fields, "member")
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(ApiError::NotFound)?;
        music_shares::revoke_grant(
            &state,
            user.current.account.id,
            &reference(&fields)?,
            member,
        )
        .await?;
    } else {
        return Err(ApiError::NotFound);
    }
    Ok(settings::redirect_to("/music/shares"))
}
pub async fn preview(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(input): Query<Preview>,
    headers: HeaderMap,
) -> Response {
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |u| u.locale);
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let json_preview = headers
        .get(header::ACCEPT)
        .is_some_and(|v| v == "application/json");
    let result=async{
        let scope=music_shares::resolve(&state,&input.token,viewer).await?;let source=music_shares::source(&state,&scope).await?;let tracks=music_shares::tracks(&state,&scope,viewer,input.max_id).await?;let title=name(&source);
        let mut tracks = tracks;
        for track in &mut tracks.items {
            if let (Some(remote),Some(entry))=(track["remote_id"].as_str(),track["entry_id"].as_str()) {
                track["stream_url"] = serde_json::json!(format!("/music/share/remote/{remote}/{entry}/stream?token={}",input.token));
            }
        }
        if json_preview {
            let mut source = source;
            for art in source["images"].as_array_mut().into_iter().flatten() {
                if let Some(id) = art["id"].as_str() {
                    art["url"] = serde_json::json!(format!("/music/share/artwork/{id}?token={}", input.token));
                }
            }
            let mut tracks = tracks;
            for track in &mut tracks.items {
                if let Some(id) = track["id"].as_str() {
                    let id = id.to_owned();
                    track["stream_url"] = serde_json::json!(format!("/music/share/assets/{id}/stream?token={}", input.token));
                    track["download_url"] = serde_json::json!(format!("/music/share/assets/{id}/download?token={}", input.token));
                }
                if let Some(id) = track["image_id"].as_i64() {
                    track["image_url"] = serde_json::json!(format!("/music/share/artwork/{id}?token={}", input.token));
                }
            }
            return Ok(crate::routes::music_shares::private_response(serde_json::json!({"source":source,"tracks":tracks})));
        }
        let body=html!{section.column.music-library{
            h1{(title)}p{(l.text("music-shares-preview-hint"))}
            @for art in source["images"].as_array().into_iter().flatten(){@if let Some(id)=art["id"].as_str(){img.music-artwork src=(format!("/music/share/artwork/{id}?token={}",input.token)) alt="" loading="lazy";}}
            @if let Some(description)=source["description"].as_str(){@if !description.is_empty(){(description_markup(description,source["description_type"].as_str()))}}
            @if matches!(scope.kind.as_str(),"library"|"playlist") {p{
                a href=(format!("/music/feeds/collections/{}.rss?token={}",scope.target_id,input.token)) rel="noreferrer"{(l.text("music-feed-rss"))}
                " · " a href=(format!("/music/feeds/collections/{}.atom?token={}",scope.target_id,input.token)) rel="noreferrer"{(l.text("music-feed-atom"))}
            }}
            @if let Some(artist)=source["artist"].as_str(){p{(artist)}}
            @if let Some(year)=source["release_year"].as_i64(){p{(year)}}
            @if !tracks.items.is_empty(){ol{@for track in tracks.items{li{
                h2{(track["title"].as_str().unwrap_or_default())}p{(track["artist"].as_str().unwrap_or_default())}
                @if let Some(image)=track["image_id"].as_i64(){img.music-artwork src=(format!("/music/share/artwork/{image}?token={}",input.token)) alt="" width="128" height="128" loading="lazy";}
                @if let Some(id)=track["id"].as_str(){audio controls preload="none" aria-label=(track["title"].as_str().unwrap_or_default()){source src=(format!("/music/share/assets/{id}/stream?token={}",input.token)) type="audio/mpeg";}p{a href=(format!("/music/share/assets/{id}/download?token={}",input.token)){(l.text("music-assets-download"))} " · " a href=(format!("/music/assets/{id}/renditions?token={}",input.token)){(l.text("music-renditions-title"))}}}
                @else if let Some(url)=track["stream_url"].as_str(){audio controls preload="none" src=(url) aria-label=(track["title"].as_str().unwrap_or_default()) {}}
            }}}}@else{p{(l.text("music-shares-no-tracks"))}}
            @if let Some(before)=tracks.before{a href=(format!("/music/share?token={}&max_id={before}",input.token)){(l.text("music-older"))}}
            p{a href="/music"{(l.text("music-title"))}}
        }};Ok::<_,ApiError>(response(title,user.as_ref(),l,&body))
    }.await;
    match result {
        Ok(response) => response,
        Err(ApiError::NotFound) if !json_preview => {
            let title = l.text("music-shares-unavailable");
            let body =
                html! {section.column{h1{(title)}p{(l.text("music-shares-unavailable-hint"))}}};
            let mut r = response(&title, user.as_ref(), l, &body);
            *r.status_mut() = axum::http::StatusCode::NOT_FOUND;
            r
        }
        Err(e) => response_error(e),
    }
}
fn response_error(error: ApiError) -> Response {
    let mut r = error.into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    r.headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    r
}

fn description_markup(text: &str, format: Option<&str>) -> Markup {
    let rendered = match format {
        Some("text/html") => plamenu_ap::text::sanitize_remote_html(text),
        Some("text/markdown") => crate::compose::markdown_to_sanitized_html(text, false),
        _ => plamenu_ap::text::plain_to_html(text),
    };
    html! {div.status__content{(maud::PreEscaped(rendered))}}
}
