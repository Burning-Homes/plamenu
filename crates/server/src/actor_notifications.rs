//! Context for managed-actor events, shared by native and Mastodon clients.
use std::collections::HashMap;

use maud::html;
use plamenu_db::{PgPool, actor_notification, notification::Notification};
use serde_json::{Value, json};

use crate::{entities::render_accounts_by_ids, error::ApiError};

fn name(account: &Value) -> &str {
    account["display_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| account["username"].as_str())
        .unwrap_or_default()
}

// Link through the serving instance. A remote Account's canonical `url` is
// untrusted input and must never become an executable href in native HTML.
fn local_profile_url(domain: &str, account: &Value) -> String {
    let acct = account["acct"].as_str().unwrap_or_default();
    let marker = if account["group"].as_bool() == Some(true) && acct.contains('@') {
        "!"
    } else {
        "@"
    };
    format!("https://{domain}/{marker}{acct}")
}

pub struct Presentation {
    pub contexts: Value,
    pub fallback: Value,
    pub title: String,
}

/// Constant query count for an entire page; recheck authority even if the
/// caller loaded its notification rows before a role was revoked.
#[allow(
    clippy::too_many_lines,
    reason = "one batched context render shared by inbox, API and push"
)]
pub async fn render(
    pool: &PgPool,
    domain: &str,
    items: &[Notification],
    viewer: i64,
) -> Result<HashMap<i64, Presentation>, ApiError> {
    let ids: Vec<i64> = items
        .iter()
        .filter(|n| n.kind.starts_with("plamenu:actor."))
        .map(|n| n.id)
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let visible: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM notifications WHERE id=ANY($1) AND account_id=$2 AND notification_visible(account_id,id,report_id)",
    ).bind(&ids).bind(viewer).fetch_all(pool).await.map_err(plamenu_db::DbError::from)?;
    let contexts = actor_notification::load(pool, &visible).await?;
    let locale = crate::web::i18n::Locale::negotiate(
        plamenu_db::user::locale_by_account_id(pool, viewer)
            .await?
            .as_deref(),
        None,
    );
    let mut account_ids: Vec<i64> = contexts
        .values()
        .flatten()
        .map(|c| c.actor_account_id)
        .chain(items.iter().map(|n| n.from_account_id))
        .collect();
    account_ids.sort_unstable();
    account_ids.dedup();
    let accounts: HashMap<i64, Value> =
        render_accounts_by_ids(pool, domain, &account_ids, Some(viewer))
            .await?
            .into_iter()
            .filter_map(|a| Some((a["id"].as_str()?.parse().ok()?, a)))
            .collect();
    let mut output = HashMap::new();
    for item in items {
        let Some(rows) = contexts.get(&item.id) else {
            continue;
        };
        let Some(sender) = accounts.get(&item.from_account_id) else {
            continue;
        };
        let mut rendered = Vec::new();
        for context in rows {
            let Some(actor) = accounts.get(&context.actor_account_id) else {
                continue;
            };
            let path = match context.event.as_str() {
                "follow_request" if context.actor_kind == "channel" => {
                    format!("/videos/channels/{}/requests", context.actor_account_id)
                }
                "follow_request" => format!("/groups/{}/requests", context.actor_account_id),
                "invitation" | "transfer" => {
                    format!("/videos/channels/{}", context.actor_account_id)
                }
                _ => format!("/@{}", actor["username"].as_str().unwrap_or_default()),
            };
            rendered.push(
                json!({"actor": actor, "kind": context.actor_kind, "event":context.event,
                "action_url":format!("https://{domain}{path}")}),
            );
        }
        // The selected reason determines the label when several real causes
        // share one notification. Channel context always takes precedence.
        let event = item.kind.strip_prefix("plamenu:actor.").unwrap_or_default();
        rendered.sort_by_key(|c| (c["kind"] != "channel", c["event"] != event));
        let Some(primary) = rendered.first() else {
            continue;
        };
        let actor = &primary["actor"];
        let detail = locale.text(&format!(
            "actor-notification-event-{}",
            primary["event"].as_str().unwrap_or_default()
        ));
        let title = format!("{} {detail} {}", name(sender), name(actor));
        let title_html = html! {
            a href=(local_profile_url(domain,sender)) {(name(sender))}
            " " (&detail) " " a href=(local_profile_url(domain,actor)) {(name(actor))}
        }
        .into_string();
        let summary = html! {
            @for context in &rendered {
                p { a href=(context["action_url"].as_str().unwrap_or_default()) {
                    @if matches!(context["event"].as_str(), Some("follow_request"|"invitation"|"transfer")) {(locale.text("actor-notification-review")) ": "}
                    @else {(locale.text("actor-profile-context")) ": "}
                    (name(&context["actor"]))
                }}
            }
        }.into_string();
        output.insert(
            item.id,
            Presentation {
                contexts: json!(rendered),
                title,
                fallback: json!({"title":title_html,"summary":summary,"details":null}),
            },
        );
    }
    Ok(output)
}

/// A scoped report is a moderation task, with the group identity and safe
/// native review destination available even to clients without report UI.
#[must_use]
pub fn report_fallback(report: &Value, domain: &str) -> Value {
    let group = report.get("group").filter(|g| g.is_object());
    let title=html! {"Report" @if let Some(group)=group {" · " a href=(local_profile_url(domain,group)) {(name(group))}}}.into_string();
    let summary =
        html! {a href=(report["action_url"].as_str().unwrap_or_default()) {"Review report"}}
            .into_string();
    json!({"title":title,"summary":summary,"details":null})
}
