//! Announcement management for the admin dashboard. The public REST API only
//! lets users read/dismiss/react; Mastodon manages create/edit/publish/destroy
//! from the admin web UI, so those writes live here. All pages require
//! `MANAGE_ANNOUNCEMENTS`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::announcement::{self, Announcement, AnnouncementUpdate, NewAnnouncement};
use plamenu_db::role::permission;
use plamenu_db::tz;

use serde::Deserialize;
use time::OffsetDateTime;

use super::super::clock::{ViewerClock, parse_datetime_local, zone_chip};
use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/announcements` — list all announcements (published, scheduled
/// and drafts) with create/edit/publish/delete forms.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ANNOUNCEMENTS)?;

    let announcements = announcement::list_all(&state.pool).await.map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That announcement could not be saved."))
        section.admin-form {
            h3 { "Create announcement" }
            form method="post" action="/web/admin/announcements" {
                input type="hidden" name="csrf" value=(csrf);
                (announcement_fields(None, admin.clock()))
                button type="submit" { "Create" }
            }
        }
        section.admin-list {
            h3 { "Announcements" }
            @if announcements.is_empty() {
                p.empty { "No announcements have been created." }
            }
            @for ann in &announcements {
                (announcement_row(ann, csrf, admin.clock()))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/announcements", "Announcements", &body).into_response())
}

fn announcement_row(ann: &Announcement, csrf: &str, clock: &ViewerClock) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong { "Announcement #" (ann.id) }
                (state_badge(ann))
            }
            form.admin-form method="post" action=(format!("/web/admin/announcements/{}/update", ann.id)) {
                input type="hidden" name="csrf" value=(csrf);
                (announcement_fields(Some(ann), clock))
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
            div.admin-actions {
                @if ann.published {
                    (op_form(ann.id, csrf, "unpublish", "Unpublish"))
                } @else {
                    (op_form(ann.id, csrf, "publish", "Publish now"))
                }
                form method="post" action=(format!("/web/admin/announcements/{}/delete", ann.id)) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Delete" }
                }
            }
        }
    }
}

fn announcement_fields(ann: Option<&Announcement>, clock: &ViewerClock) -> Markup {
    let text = ann.map_or("", |a| a.text.as_str());
    // Echoed back as the wall-clock reading the admin typed, not the stored
    // instant: a value that round-trips through the form must not drift.
    let scheduled_at = ann
        .and_then(|a| a.scheduled_at)
        .map_or_else(String::new, |at| clock.input_value(at));
    let starts_at = ann
        .and_then(|a| a.starts_at)
        .map_or_else(String::new, |at| clock.input_value(at));
    let ends_at = ann
        .and_then(|a| a.ends_at)
        .map_or_else(String::new, |at| clock.input_value(at));
    let status_ids = ann
        .and_then(|a| a.status_ids.as_ref())
        .map(|ids| {
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let all_day = ann.is_some_and(|a| a.all_day);
    html! {
        label {
            "Text"
            textarea name="text" rows="3" required { (text) }
        }
        div.admin-form__grid {
            label {
                "Scheduled at"
                input type="datetime-local" name="scheduled_at" value=(scheduled_at);
            }
            label {
                "Starts at"
                input type="datetime-local" name="starts_at" value=(starts_at);
            }
            label {
                "Ends at"
                input type="datetime-local" name="ends_at" value=(ends_at);
            }
        }
        (zone_chip(clock))
        label.admin-check {
            input type="checkbox" name="all_day" value="1" checked[all_day];
            span { "All day" }
        }
        label {
            "Status ids"
            input type="text" name="status_ids" value=(status_ids) placeholder="123, 456";
            span.settings-field__hint {
                "Local post ids to cite: public/unlisted posts embed under the announcement."
            }
        }
    }
}

fn op_form(id: i64, csrf: &str, op: &str, label: &str) -> Markup {
    html! {
        form method="post" action=(format!("/web/admin/announcements/{id}/op")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="op" value=(op);
            button type="submit" { (label) }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AnnouncementForm {
    csrf: String,
    text: String,
    #[serde(default)]
    scheduled_at: String,
    #[serde(default)]
    starts_at: String,
    #[serde(default)]
    ends_at: String,
    all_day: Option<String>,
    #[serde(default)]
    status_ids: String,
}

/// `POST /web/admin/announcements` — create an announcement. It publishes
/// immediately unless `scheduled_at` is in the future, matching `db::create`.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<AnnouncementForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ANNOUNCEMENTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(data) = parse_form(&state.pool, &form, admin.clock()).await else {
        return Ok(redirect_announcements("error"));
    };
    let ann = announcement::create(
        &state.pool,
        NewAnnouncement {
            text: data.text,
            scheduled_at: data.scheduled_at,
            starts_at: data.starts_at,
            ends_at: data.ends_at,
            all_day: data.all_day,
            status_ids: data.status_ids.as_deref(),
        },
    )
    .await
    .map_err(api_err)?;
    if ann.published {
        crate::streaming::announcement_published(&state, ann.id).await;
    }
    log_announcement(&state, &admin, "create", &ann).await?;
    Ok(redirect_announcements("applied"))
}

/// `POST /web/admin/announcements/{id}/update` — edit fields in place without
/// changing the publish state.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<AnnouncementForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ANNOUNCEMENTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(data) = parse_form(&state.pool, &form, admin.clock()).await else {
        return Ok(redirect_announcements("error"));
    };
    let updated = announcement::update(
        &state.pool,
        id,
        AnnouncementUpdate {
            text: data.text,
            scheduled_at: data.scheduled_at,
            starts_at: data.starts_at,
            ends_at: data.ends_at,
            all_day: data.all_day,
            status_ids: data.status_ids.as_deref(),
        },
    )
    .await
    .map_err(api_err)?;
    if let Some(ann) = &updated {
        log_announcement(&state, &admin, "update", ann).await?;
    }
    Ok(redirect_announcements(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/announcements/{id}/op` — publish or unpublish.
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ANNOUNCEMENTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    match form.op.as_str() {
        "publish" => {
            let Some(ann) = announcement::publish(&state.pool, id)
                .await
                .map_err(api_err)?
            else {
                return Ok(redirect_announcements("error"));
            };
            crate::streaming::announcement_published(&state, ann.id).await;
            log_announcement(&state, &admin, "publish", &ann).await?;
        }
        "unpublish" => {
            let Some(ann) = announcement::unpublish(&state.pool, id)
                .await
                .map_err(api_err)?
            else {
                return Ok(redirect_announcements("error"));
            };
            crate::streaming::announcement_deleted(&state, ann.id).await;
            log_announcement(&state, &admin, "unpublish", &ann).await?;
        }
        _ => return Ok(redirect_announcements("error")),
    }
    Ok(redirect_announcements("applied"))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/announcements/{id}/delete` — destroy an announcement and
/// tell live clients to remove it.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ANNOUNCEMENTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = announcement::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = announcement::delete(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted {
        crate::streaming::announcement_deleted(&state, id).await;
        if let Some(ann) = &target {
            log_announcement(&state, &admin, "destroy", ann).await?;
        }
    }
    Ok(redirect_announcements(if deleted {
        "applied"
    } else {
        "error"
    }))
}

struct ParsedAnnouncementForm<'a> {
    text: &'a str,
    scheduled_at: Option<OffsetDateTime>,
    starts_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    all_day: bool,
    status_ids: Option<Vec<i64>>,
}

/// Reads the form, resolving each wall-clock reading the admin typed into the
/// instant it names *in their own zone* — "publish at 9:00 Monday" means 9:00
/// on their clock, not 9:00 UTC. The conversion goes through Postgres's IANA
/// database ([`tz::local_to_utc`]) like the composer's Schedule field, so DST
/// transitions resolve by the maintained rules rather than a fixed offset.
///
/// `None` means the submission was malformed; the caller redirects with the
/// error flash.
async fn parse_form<'a>(
    pool: &plamenu_db::PgPool,
    form: &'a AnnouncementForm,
    clock: &ViewerClock,
) -> Option<ParsedAnnouncementForm<'a>> {
    let text = form.text.trim();
    if text.is_empty() {
        return None;
    }
    let status_ids = parse_status_ids(&form.status_ids)?;
    Some(ParsedAnnouncementForm {
        text,
        scheduled_at: parse_time(pool, &form.scheduled_at, clock).await?,
        starts_at: parse_time(pool, &form.starts_at, clock).await?,
        ends_at: parse_time(pool, &form.ends_at, clock).await?,
        all_day: form.all_day.is_some(),
        status_ids: (!status_ids.is_empty()).then_some(status_ids),
    })
}

/// An optional `datetime-local` field: `Some(None)` for a blank field (a
/// legitimate "unset"), `None` for one that could not be read or resolved.
async fn parse_time(
    pool: &plamenu_db::PgPool,
    raw: &str,
    clock: &ViewerClock,
) -> Option<Option<OffsetDateTime>> {
    if raw.trim().is_empty() {
        return Some(None);
    }
    let local = parse_datetime_local(raw)?;
    // `clock.name()` is a `&'static str` from the zone inventory, which is what
    // makes interpolating it into `AT TIME ZONE` safe.
    tz::local_to_utc(pool, local, clock.name())
        .await
        .ok()
        .map(Some)
}

fn parse_status_ids(raw: &str) -> Option<Vec<i64>> {
    raw.split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn state_badge(ann: &Announcement) -> Markup {
    let (label, class) = if ann.published {
        ("Published", "is-active")
    } else if ann.scheduled_at.is_some() {
        ("Scheduled", "is-pending")
    } else {
        ("Unpublished", "is-disabled")
    };
    html! { span.admin-badge class=(format!("admin-badge {class}")) { (label) } }
}

fn redirect_announcements(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/announcements?flash={flash}"),
        )],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}

/// Appends an announcement verb to the audit log.
async fn log_announcement(
    state: &AppState,
    admin: &WebAdmin,
    verb: &str,
    ann: &Announcement,
) -> Result<(), Response> {
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        verb,
        &admin_log::Target::announcement(ann.id, &ann.text),
    )
    .await
    .map_err(api_err)?;
    Ok(())
}
