//! Compact, HTML-first relationships between people and managed actors.
use super::{i18n::Locale, session::WebUser, view};
use crate::{AppState, error::ApiError};
use maud::{Markup, html};
use plamenu_db::{account::Account, managed_actor};

pub async fn panel(
    state: &AppState,
    account: &Account,
    user: Option<&WebUser>,
    locale: Locale,
) -> Result<Markup, ApiError> {
    let kind = managed_actor::kind(&state.pool, account.id).await?;
    let rows = if kind.is_some() {
        managed_actor::roles(&state.pool, account.id).await?
    } else if !account.is_group() {
        managed_actor::connections(&state.pool, account.id).await?
    } else {
        Vec::new()
    };
    if rows.is_empty() {
        return Ok(html! {});
    }
    let viewer = user.map(|u| u.current.account.id);
    let role = rows
        .iter()
        .find(|r| Some(r.account_id) == viewer)
        .map(|r| r.role.as_str());
    let ids: Vec<i64> = rows.iter().map(|r| r.account_id).collect();
    let cards =
        crate::entities::render_accounts_by_ids(&state.pool, &state.config.domain, &ids, viewer)
            .await?;
    let channel = kind.as_deref() == Some("channel");
    let group = kind.as_deref() == Some("group");
    let can_manage = kind.is_some() && role.is_some();
    let can_edit = matches!(role, Some("owner" | "editor"));
    Ok(html! {
        aside.actor-connections {
            details.actor-connections__people {
                summary {(locale.text(if kind.is_some(){"actor-profile-controllers"}else{"actor-profile-connections"})) " (" (rows.len()) ")"}
                ul {
                    @for row in &rows {
                        @if let Some(value)=cards.iter().find(|v|v["id"].as_str()==Some(row.account_id.to_string().as_str())) {
                            @let person=view::Account(value);
                            li {
                                a href=(person.profile_path()) {
                                    (person.name_markup())
                                    @if value["display_name"].as_str().is_some_and(|name|!name.is_empty() && name!=person.acct()) {
                                        small {" @" (person.display_acct())}
                                    }
                                }
                                " · " (locale.text(&format!("actor-role-{}",row.role)))
                            }
                        }
                    }
                }
            }
            @if can_manage {
                details.actor-connections__manage {
                    summary {(locale.text("actor-profile-manage"))}
                    nav aria-label=(locale.text("actor-profile-manage")) {
                        @if channel {
                            a href=(format!("/videos/channels/{}",account.id)) {(locale.text("video-channel-manage"))}
                            a href=(format!("/videos/upload?channel={}",account.id)) {(locale.text("videos-upload-review"))}
                            a href=(format!("/videos?channel={}",account.id)) {(locale.text("studio-library"))}
                            a href=(format!("/videos/live/new?channel={}",account.id)) {(locale.text("actor-profile-go-live"))}
                            @if can_edit {
                                a href=(format!("/videos/channels/{}/requests",account.id)) {(locale.text("actor-profile-follow-requests"))}
                                a href=(format!("/videos/channels/{}/analytics",account.id)) {(locale.text("video-analytics-title"))}
                            }
                        } @else if group {
                            a href=(format!("/groups/{}/manage",account.id)) {(locale.text("groups-tab-settings"))}
                            a href=(format!("/groups/{}/members",account.id)) {(locale.text("groups-tab-members"))}
                            a href=(format!("/groups/{}/requests",account.id)) {(locale.text("groups-tab-requests"))}
                            a href=(format!("/groups/{}/reports",account.id)) {(locale.text("actor-profile-reports"))}
                            @if role==Some("owner") { a href=(format!("/groups/{}/moderators",account.id)) {(locale.text("groups-tab-moderators"))} }
                        }
                    }
                }
            }
        }
    })
}

#[derive(Default, serde::Deserialize)]
pub struct ChannelSelection {
    pub channel: Option<i64>,
}
impl ChannelSelection {
    pub async fn validate(&self, state: &AppState, user: &WebUser) -> Result<(), ApiError> {
        if let Some(id) = self.channel
            && plamenu_db::video_channel_access::role(&state.pool, id, user.current.account.id)
                .await?
                .is_none()
        {
            return Err(ApiError::NotFound);
        }
        Ok(())
    }
}
