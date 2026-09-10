//! Trend review for the admin dashboard — the web face of the trend data
//! admin trends REST surface (Mastodon's `admin/trends/*` pages). Trending
//! statuses, links, link publishers and hashtags are listed highest-score
//! first with approve/reject verbs; approving marks the item trendable so it
//! surfaces publicly after the next trends refresh. All pages require
//! `MANAGE_TAXONOMIES`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::{preview_card_provider, preview_card_trend, status_trend, tag, tag_trend};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::web::session::csrf_rejection;
use crate::web::view;

/// Rows shown per section — trends are a ranked shortlist, not an archive.
const SECTION_LIMIT: i64 = 20;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/trends` — the review queues: trending statuses, links,
/// publishers and hashtags, each with inline approve/reject.
#[allow(clippy::too_many_lines, reason = "one linear review-queue assembly")]
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_TAXONOMIES)?;

    let statuses = status_trend::all_admin(&state.pool, SECTION_LIMIT, 0)
        .await
        .map_err(api_err)?;
    let status_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    let unreviewed = status_trend::unreviewed_ids(&state.pool, &status_ids)
        .await
        .map_err(api_err)?;
    // Full post entities so the review queue shows a proper card (author, media,
    // content warning, poll) rather than bare content. No viewer: the preview is
    // read-only and every trending post is public.
    let entities =
        crate::entities::render_statuses(&state.pool, &state.config.domain, &statuses, None)
            .await
            .map_err(IntoResponse::into_response)?;
    let links = preview_card_trend::all_admin(&state.pool, SECTION_LIMIT, 0)
        .await
        .map_err(api_err)?;
    let link_ids: Vec<i64> = links.iter().map(|c| c.id).collect();
    let link_unreviewed = preview_card_trend::unreviewed_ids(&state.pool, &link_ids)
        .await
        .map_err(api_err)?;
    let tags = tag_trend::all_admin(&state.pool, SECTION_LIMIT, 0)
        .await
        .map_err(api_err)?;
    let publishers = preview_card_provider::list(&state.pool, None, None, None, SECTION_LIMIT)
        .await
        .map_err(api_err)?;

    let clock = &admin.user.clock;
    let ctx = view::Ctx {
        csrf: None,
        viewer_id: None,
        return_to: "/admin/trends",
        filter_context: None,
        prefs: view::ViewPrefs::default(),
        locale: admin.user.locale,
        clock: clock.clone(),
        admin: admin.user.admin_capabilities(),
    };
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That review could not be saved."))
        p.admin__lead {
            "Review what is allowed to trend. Approving marks the item trendable "
            "— it surfaces publicly after the next trends refresh. A rejected "
            "post or link leaves this queue; a rejected publisher or hashtag "
            "stays listed, marked as rejected."
        }
        section.admin-list {
            h3 { "Trending posts" }
            @if entities.is_empty() { p.empty { "Nothing is trending." } }
            @for entity in &entities {
                @let status = view::Status(entity);
                @let sid = status.id().parse::<i64>().unwrap_or_default();
                article.admin-record {
                    div.admin-record__head {
                        @if unreviewed.contains(&sid) {
                            span.admin-badge.is-pending { "Pending review" }
                        } @else {
                            span.admin-badge.is-active { "Approved" }
                        }
                        a.admin-record__open href=(format!("/web/statuses/{sid}"))
                            target="_blank" rel="noopener" { "Open in Plamenu" }
                    }
                    div.admin-trend-preview {
                        (view::status_preview(&status, &ctx))
                    }
                    (review_forms("statuses", sid, csrf))
                }
            }
        }
        section.admin-list {
            h3 { "Trending links" }
            @if links.is_empty() { p.empty { "No links are trending." } }
            @for card in &links {
                article.admin-record {
                    div.admin-record__head {
                        strong { (card.title) }
                        @if link_unreviewed.contains(&card.id) {
                            span.admin-badge.is-pending { "Pending review" }
                        } @else {
                            span.admin-badge.is-active { "Approved" }
                        }
                    }
                    p.admin-table__sub { (card.provider_name) }
                    p { a href=(card.url) target="_blank" rel="noopener" { (card.url) } }
                    (review_forms("links", card.id, csrf))
                }
            }
        }
        section.admin-list {
            h3 { "Link publishers" }
            @if publishers.is_empty() { p.empty { "No publishers recorded." } }
            @for provider in &publishers {
                article.admin-record {
                    div.admin-record__head {
                        strong { (provider.domain) }
                        (trendable_badge(provider.trendable))
                    }
                    (review_forms("publishers", provider.id, csrf))
                }
            }
        }
        section.admin-list {
            h3 { "Trending hashtags" }
            @if tags.is_empty() { p.empty { "No hashtags are trending." } }
            @for tag in &tags {
                article.admin-record {
                    div.admin-record__head {
                        strong { "#" (tag.display()) }
                        (trendable_badge(tag.trendable))
                    }
                    (review_forms("tags", tag.id, csrf))
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/trends", "Trends", &body).into_response())
}

/// The paired approve/reject forms for one reviewable item.
fn review_forms(kind: &str, id: i64, csrf: &str) -> Markup {
    html! {
        div.admin-actions {
            form method="post" action=(format!("/web/admin/trends/{kind}/{id}/review")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="approve";
                button type="submit" { "Approve" }
            }
            form method="post" action=(format!("/web/admin/trends/{kind}/{id}/review")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="reject";
                button.admin-danger type="submit" { "Reject" }
            }
        }
    }
}

fn trendable_badge(trendable: Option<bool>) -> Markup {
    html! {
        @match trendable {
            Some(true) => span.admin-badge.is-active { "Approved" }
            Some(false) => span.admin-badge.is-disabled { "Rejected" }
            None => span.admin-badge.is-pending { "Pending review" }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReviewForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/trends/{kind}/{id}/review` — approve or reject one
/// trending status/link/publisher/hashtag, mirroring the REST verbs.
pub async fn review(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path((kind, id)): Path<(String, i64)>,
    Form(form): Form<ReviewForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_TAXONOMIES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let approve = match form.op.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return Ok(redirect_trends("error")),
    };
    let found = match kind.as_str() {
        "statuses" => status_trend::set_trendable(&state.pool, id, approve)
            .await
            .map_err(api_err)?,
        "links" => preview_card_trend::set_trendable(&state.pool, id, approve)
            .await
            .map_err(api_err)?,
        "publishers" => preview_card_provider::set_trendable(
            &state.pool,
            id,
            approve,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .map_err(api_err)?
        .is_some(),
        "tags" => tag::admin_update(
            &state.pool,
            id,
            None,
            None,
            None,
            Some(approve),
            time::OffsetDateTime::now_utc(),
        )
        .await
        .map_err(api_err)?
        .is_some(),
        _ => return Ok(redirect_trends("error")),
    };
    Ok(redirect_trends(if found { "applied" } else { "error" }))
}

fn redirect_trends(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/trends?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
