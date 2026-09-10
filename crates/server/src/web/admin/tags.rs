//! Hashtag moderation for the admin dashboard (Mastodon's `admin/tags`) — the
//! web face of `GET/PUT /api/v1/admin/tags`. Each tag's moderation registry
//! (`usable`/`listable`/`trendable` plus the display-casing override) is
//! editable inline; saving stamps `reviewed_at`, exactly like the REST
//! update. All pages require `MANAGE_TAXONOMIES`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::tag::{self, AdminTag};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::web::session::csrf_rejection;

/// Page size for the tag listing (Mastodon's admin tags LIMIT).
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    max_id: Option<i64>,
    flash: Option<String>,
}

/// `GET /admin/tags` — every known tag with its moderation registry,
/// id-keyset paginated newest first.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_TAXONOMIES)?;

    let tags = tag::admin_list(&state.pool, query.max_id, None, None, PAGE_LIMIT)
        .await
        .map_err(api_err)?;
    let next_max = (i64::try_from(tags.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| tags.last().map(|tag| tag.id))
        .flatten();

    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That tag could not be saved."))
        p.admin__lead {
            "The moderation registry for hashtags: whether each can be used in "
            "posts, listed on public timelines, and allowed to trend. Saving "
            "marks the tag as reviewed."
        }
        section.admin-list data-paged {
            @if tags.is_empty() {
                p.empty { "No hashtags have been used yet." }
            }
            @for tag in &tags {
                (tag_row(tag, csrf))
            }
        }
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(format!("/admin/tags?max_id={max}")) { "Older →" }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/tags", "Hashtags", &body).into_response())
}

fn tag_row(tag: &AdminTag, csrf: &str) -> Markup {
    html! {
        article.admin-record {
            form.admin-form method="post" action=(format!("/web/admin/tags/{}/update", tag.id)) {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-record__head {
                    strong { "#" (tag.display()) }
                    @if tag.reviewed_at.is_some() {
                        span.admin-badge.is-active { "Reviewed" }
                    } @else {
                        span.admin-badge.is-pending { "Unreviewed" }
                    }
                }
                label {
                    "Display name"
                    input type="text" name="display_name"
                        value=(tag.display_name.as_deref().unwrap_or(""))
                        placeholder=(tag.name);
                }
                div.admin-form__group {
                    label.admin-check {
                        input type="checkbox" name="usable" value="1"
                            checked[tag.usable.unwrap_or(true)];
                        span { "Allow in posts" }
                    }
                    label.admin-check {
                        input type="checkbox" name="listable" value="1"
                            checked[tag.listable.unwrap_or(true)];
                        span { "Show on public timelines and in search" }
                    }
                    label.admin-check {
                        input type="checkbox" name="trendable" value="1"
                            checked[tag.trendable.unwrap_or(false)];
                        span { "Allow to trend" }
                    }
                }
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateForm {
    csrf: String,
    #[serde(default)]
    display_name: String,
    usable: Option<String>,
    listable: Option<String>,
    trendable: Option<String>,
}

/// `POST /web/admin/tags/{id}/update` — set the registry fields and stamp
/// `reviewed_at`.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_TAXONOMIES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let display_name = form.display_name.trim();
    let updated = tag::admin_update(
        &state.pool,
        id,
        (!display_name.is_empty()).then_some(display_name),
        Some(form.usable.is_some()),
        Some(form.listable.is_some()),
        Some(form.trendable.is_some()),
        time::OffsetDateTime::now_utc(),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_tags(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

fn redirect_tags(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/tags?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
