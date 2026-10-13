//! Explicit seen/unseen controls do not mark a page read merely by opening it.
use super::{layout, pages, session::WebUser, settings, view};
use crate::{AppState, error::ApiError, routes::notification_inbox::Page};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{notification, notification_read};
#[allow(clippy::too_many_lines)] // one flat inbox template keeps controls with their read state
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:notifications")?;
    let kinds = page.kinds()?;
    let unread = page.unread.unwrap_or(false);
    let mut rows = notification_read::list(
        &state.pool,
        user.current.user.id,
        user.current.account.id,
        notification_read::Filter {
            before: page.before,
            unread,
            mail: page.mail.unwrap_or(false),
            kinds: kinds.as_deref(),
            exclude_direct: !page.mail.unwrap_or(false),
        },
    )
    .await?;
    let before = if rows.len() > 40 {
        rows.truncate(40);
        rows.last().map(|n| n.id)
    } else {
        None
    };
    let states = notification_read::read_states(
        &state.pool,
        user.current.user.id,
        user.current.account.id,
        &rows.iter().map(|n| n.id).collect::<Vec<_>>(),
    )
    .await?;
    let items = crate::entities::render_notifications(
        &state.pool,
        &state.config.domain,
        &rows,
        user.current.account.id,
    )
    .await?;
    let back = format!(
        "/notifications?{}",
        serde_urlencoded::to_string([
            ("kind", page.kind.as_deref().unwrap_or_default()),
            ("unread", if unread { "true" } else { "false" }),
            (
                "mail",
                if page.mail.unwrap_or(false) {
                    "true"
                } else {
                    "false"
                }
            ),
        ])
        .unwrap_or_default()
    );
    let viewer = user.current.account.id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: &back,
        filter_context: Some(view::FilterContext::Notifications),
        prefs: pages::view_prefs(&state, Some(&user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let l = user.locale;
    let body = html! {section.column.activity-inbox {
        header.inbox-heading {
            h1 {(l.text("page-notifications"))}
            form method="post" action="/web/notifications/inbox" {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="return_to" value=(&back);
                button.button--quiet name="operation" value="all" {(l.text("notification-inbox-seen-all"))}
            }
            details.inbox-options {
                summary {(l.text("inbox-options"))}
                div.inbox-options__body {
                    a href="/settings/notification-delivery" {(l.text("notification-delivery-title"))}
                    a href="/notifications/inbox?mail=true" {(l.text("notification-delivery-email"))}
                    a href="/notifications/requests" {(l.text("settings-privacy-filtered-notifications"))}
                    p {(l.text("notification-inbox-hint"))}
                    details {
                        summary {(l.text("notification-inbox-delete-all"))}
                        form method="post" action="/web/notifications/inbox" {
                            input type="hidden" name="csrf" value=(user.csrf);
                            input type="hidden" name="return_to" value=(&back);
                            label {input type="checkbox" name="confirm" value="true" required; " " (l.text("notification-inbox-confirm"))}
                            button.settings-button--danger name="operation" value="clear" {(l.text("notification-inbox-delete-all"))}
                        }
                    }
                }
            }
        }
        nav.inbox-tabs aria-label=(l.text("page-notifications")) {
            a href="/notifications" aria-current=[(!unread && page.kind.as_deref().is_none_or(str::is_empty) && !page.mail.unwrap_or(false)).then_some("page")] {(l.text("notification-inbox-all"))}
            a href="/notifications?unread=true" aria-current=[unread.then_some("page")] {(l.text("notification-inbox-unseen"))}
            a href="/notifications?kind=mention" aria-current=[(page.kind.as_deref()==Some("mention")).then_some("page")] {(l.text("mentions-title"))}
        }
        form.inbox-filter method="get" action="/notifications" {
            @if unread {input type="hidden" name="unread" value="true";}
            @if page.mail.unwrap_or(false) {input type="hidden" name="mail" value="true";}
            label.visually-hidden for="notification-kind" {(l.text("notification-inbox-type"))}
            select id="notification-kind" name="kind" {
                option value="" {(l.text("notification-inbox-all"))}
                @for kind in crate::routes::notifications::TYPES {
                    option value=(kind) selected[page.kind.as_deref()==Some(kind)] {(super::notification_delivery::label(l,kind))}
                }
            }
            button.button--quiet type="submit" {(l.text("inbox-filter"))}
        }
        @if page.mail.unwrap_or(false) {p {(l.text("notification-delivery-email"))}}
        @if items.is_empty() {p.empty {(l.text("page-notifications-empty"))}}
        div.notifications data-paged {
            @for note in &items {
                @let content=view::notification_item(note,&ctx);
                @if let Some(id)=note["id"].as_str().and_then(|id|id.parse::<i64>().ok()) && !content.0.is_empty() {
                    @let seen=states.get(&id).copied().unwrap_or(false);
                    div.notification-inbox-item data-seen=(seen) {
                        (content)
                        footer.notification-inbox-actions {
                            form method="post" action="/web/notifications/inbox" {
                                input type="hidden" name="csrf" value=(user.csrf);
                                input type="hidden" name="return_to" value=(&back);
                                input type="hidden" name="id" value=(id);
                                button.button--quiet name="operation" value=(if seen {"unseen"} else {"seen"}) {
                                    (l.text(if seen {"notification-inbox-mark-unseen"} else {"notification-inbox-mark-seen"}))
                                }
                            }
                            details.inbox-item-menu {
                                summary aria-label=(l.text("inbox-options")) {(view::icon("more"))}
                                form method="post" action="/web/notifications/inbox" {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    input type="hidden" name="return_to" value=(&back);
                                    input type="hidden" name="id" value=(id);
                                    button.button--quiet name="operation" value="delete" {(l.text("notification-inbox-delete"))}
                                }
                            }
                        }
                    }
                }
            }
        }
        @if let Some(before)=before {nav.pager {a.pager__more href=(format!("{back}&before={before}")) {(l.text("pager-older"))}}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("page-notifications"), Some(&user), &body),
    )
        .into_response())
}
pub async fn action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:notifications")?;
    match settings::field(&pairs, "operation").unwrap_or_default() {
        "all" => {
            notification_read::mark_activity_read(&state.pool, user.current.account.id).await?;
        }
        "clear" => {
            if settings::field(&pairs, "confirm") != Some("true") {
                return Err(ApiError::Unprocessable(
                    "Confirm deleting all notifications".into(),
                ));
            }
            notification::clear(&state.pool, user.current.account.id).await?;
        }
        op @ ("seen" | "unseen" | "delete") => {
            let id = settings::field(&pairs, "id")
                .and_then(|id| id.parse().ok())
                .ok_or(ApiError::NotFound)?;
            let changed = if op == "delete" {
                notification::dismiss(&state.pool, user.current.account.id, id).await?
            } else {
                notification_read::set_read(&state.pool, user.current.account.id, id, op == "seen")
                    .await?
            };
            if !changed {
                return Err(ApiError::NotFound);
            }
        }
        _ => return Err(ApiError::Unprocessable("Unknown inbox operation".into())),
    }
    let back =
        super::actions::safe_return(settings::field(&pairs, "return_to"), "/notifications/inbox");
    Ok(settings::redirect_to(&back))
}

pub async fn unread_state(user: WebUser) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        axum::Json(serde_json::json!({
            "account": user.current.account.id.to_string(),
            "notifications": user.unread_notifications,
            "messages": user.unread_conversations,
        })),
    )
        .into_response()
}
