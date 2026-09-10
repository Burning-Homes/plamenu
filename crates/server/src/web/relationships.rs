//! Relationships manager — Mastodon's `/relationships` page. Lists the
//! accounts you follow, the accounts that follow you, or the mutual set, plus
//! the accounts you've blocked or muted and the domains you've blocked, and
//! lets you act on several at once: bulk-unfollow from the following/mutual
//! views, bulk remove-follower from the followers view, bulk unblock/unmute
//! from the blocked/muted views, and bulk domain-unblock from the domains view.
//! The requests view lists pending follow requests *received* toward a locked
//! account with bulk accept/reject, mirroring `/api/v1/follow_requests`; the
//! sent-requests view is its outgoing mirror — follows *you* initiated that are
//! still awaiting the target's `Accept` — with bulk cancel (a plain unfollow).
//! Each action reuses the same services the REST API does, so federation
//! happens exactly as usual.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::{account, account_domain_block, block, follow, mute, tag};
use serde::Deserialize;

use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, field, form_pairs, redirect_to, saved_flash, settings_shell};
use super::view;
use crate::actions;
use crate::entities::render_accounts_by_ids;
use crate::error::ApiError;
use crate::state::AppState;

/// Page size for the list, matching the follow-list pages.
const LIMIT: i64 = 40;

/// Whether a page came back full — i.e. there may be more rows to load.
fn page_full(count: usize) -> bool {
    count >= usize::try_from(LIMIT).unwrap_or(usize::MAX)
}

/// A tab label whose message parenthesises the count when there's anything
/// pending, e.g. `Sent requests (3)`, and drops it when the count is zero.
fn badge_label(id: &str, count: i64, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("count", count);
    locale.text_with(id, &args)
}

/// Which set of relationships the page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Relationship {
    Following,
    Followers,
    Requests,
    SentRequests,
    Mutual,
    Hashtags,
    Blocked,
    Muted,
    BlockedDomains,
}

impl Relationship {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("followers") => Self::Followers,
            Some("requests") => Self::Requests,
            Some("sent-requests") => Self::SentRequests,
            Some("mutual") => Self::Mutual,
            Some("hashtags") => Self::Hashtags,
            Some("blocked") => Self::Blocked,
            Some("muted") => Self::Muted,
            Some("blocked-domains") => Self::BlockedDomains,
            _ => Self::Following,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Followers => "followers",
            Self::Requests => "requests",
            Self::SentRequests => "sent-requests",
            Self::Mutual => "mutual",
            Self::Hashtags => "hashtags",
            Self::Blocked => "blocked",
            Self::Muted => "muted",
            Self::BlockedDomains => "blocked-domains",
        }
    }

    /// The bulk action offered for this view, and the catalog message naming
    /// its button.
    fn bulk(self) -> (&'static str, &'static str) {
        match self {
            // Removing a follower only makes sense on the followers view; the
            // following/mutual views act on people *you* follow.
            Self::Followers => ("remove_follower", "relationships-bulk-remove-followers"),
            // The requests view renders its own accept/reject button pair;
            // this names only the fallback action.
            Self::Requests => ("authorize", "relationships-bulk-accept"),
            // Cancelling a sent request is just withdrawing the follow.
            Self::SentRequests => ("unfollow", "relationships-bulk-cancel-requests"),
            Self::Hashtags => ("unfollow_tag", "relationships-bulk-unfollow-hashtags"),
            Self::Blocked => ("unblock", "relationships-bulk-unblock"),
            Self::Muted => ("unmute", "relationships-bulk-unmute"),
            Self::BlockedDomains => ("unblock_domain", "relationships-bulk-unblock-domains"),
            Self::Following | Self::Mutual => ("unfollow", "relationships-bulk-unfollow"),
        }
    }

    /// The catalog message for a view with no rows.
    fn empty_text(self) -> &'static str {
        match self {
            Self::Requests => "relationships-empty-requests",
            Self::SentRequests => "relationships-empty-sent",
            Self::Hashtags => "relationships-empty-hashtags",
            Self::Blocked => "relationships-empty-blocked",
            Self::Muted => "relationships-empty-muted",
            Self::BlockedDomains => "relationships-empty-blocked-domains",
            _ => "relationships-empty",
        }
    }
}

#[derive(Deserialize)]
pub struct RelationshipsQuery {
    rel: Option<String>,
    /// Offset cursor for the follow-derived views (following/followers/mutual).
    offset: Option<i64>,
    /// Keyset cursor (last row id) for the block/mute/domain views.
    max_id: Option<i64>,
    saved: Option<String>,
}

/// The rows a view renders: resolved account cards, plain domain names, or
/// followed hashtags.
enum Rows {
    Accounts(Vec<serde_json::Value>),
    Domains(Vec<String>),
    Tags(Vec<tag::FollowedTag>),
}

impl Rows {
    fn is_empty(&self) -> bool {
        match self {
            Self::Accounts(values) => values.is_empty(),
            Self::Domains(domains) => domains.is_empty(),
            Self::Tags(tags) => tags.is_empty(),
        }
    }
}

/// `GET /settings/relationships` — the current relationship set as a bulk-action
/// form of account (or domain) rows.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<RelationshipsQuery>,
) -> Response {
    let relationship = Relationship::parse(query.rel.as_deref());
    let account_id = user.current.account.id;
    let offset = query.offset.unwrap_or(0).max(0);
    let max_id = query.max_id;

    // The blocked-domains view lists server names and the hashtags view lists
    // followed tags rather than accounts, so those fetch and render
    // differently; every other view renders account rows.
    let result = match relationship {
        Relationship::BlockedDomains => domain_rows(&state, relationship, account_id, max_id).await,
        Relationship::Hashtags => tag_rows(&state, relationship, account_id, max_id).await,
        _ => account_rows(&state, relationship, account_id, offset, max_id).await,
    };
    let (rows, next_href) = match result {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };

    // Mastodon's serializer caps the surfaced request count at 40; mirror the
    // cap on the sent side so both badges read the same way.
    let received = match follow::count_requests(&state.pool, account_id, 40).await {
        Ok(count) => count,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let sent = match follow::count_sent_requests(&state.pool, account_id, 40).await {
        Ok(count) => count,
        Err(err) => return ApiError::from(err).into_response(),
    };
    // Received vs sent are named explicitly so the two pending directions can't
    // be mistaken for one another.
    let locale = user.locale;
    let views = tab_strip(relationship, received, sent, locale);

    let (action, button_label) = relationship.bulk();
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("relationships-saved")))
        p.settings-field__hint { (locale.text("relationships-intro")) }
        (views)
        @if relationship == Relationship::SentRequests {
            p.settings-field__hint { (locale.text("relationships-sent-hint")) }
        }
        @if rows.is_empty() {
            p.empty { (locale.text(relationship.empty_text())) }
        } @else {
            form.settings-form method="post" action="/web/settings/relationships" {
                input type="hidden" name="csrf" value=(user.csrf);
                @if relationship != Relationship::Requests {
                    input type="hidden" name="action" value=(action);
                }
                input type="hidden" name="rel" value=(relationship.slug());
                ul.relationships-list data-paged {
                    @match &rows {
                        Rows::Accounts(values) => {
                            @for value in values { (account_row(value)) }
                        }
                        Rows::Domains(domains) => {
                            @for domain in domains { (domain_row(domain)) }
                        }
                        Rows::Tags(tags) => {
                            @for tag in tags { (tag_row(tag)) }
                        }
                    }
                }
                div.settings-form__actions {
                    @if relationship == Relationship::Requests {
                        button type="submit" name="action" value="authorize" {
                            (locale.text("relationships-bulk-accept"))
                        }
                        button type="submit" name="action" value="reject" {
                            (locale.text("relationships-bulk-reject"))
                        }
                    } @else {
                        button type="submit" { (locale.text(button_label)) }
                    }
                }
            }
        }
        @if let Some(href) = &next_href {
            nav.pager {
                a.pager__more href=(href) { (locale.text("page-load-more")) }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/relationships",
        &locale.text("settings-section-relationships"),
        &body,
    )
    .into_response()
}

/// The view selector. Each label is materialized here because `view::Tab`
/// borrows its label for the lifetime of the strip.
fn tab_strip(current: Relationship, received: i64, sent: i64, locale: Locale) -> Markup {
    let received_label = badge_label("relationships-tab-received", received, locale);
    let sent_label = badge_label("relationships-tab-sent", sent, locale);
    let following_label = locale.text("profile-following");
    let followers_label = locale.text("profile-followers");
    let mutual_label = locale.text("relationships-tab-mutual");
    let hashtags_label = locale.text("relationships-tab-hashtags");
    let blocked_label = locale.text("relationships-tab-blocked");
    let muted_label = locale.text("relationships-tab-muted");
    let domains_label = locale.text("relationships-tab-blocked-domains");
    view::tab_strip(
        &locale.text("relationships-views"),
        &[
            view::Tab::new(
                "/settings/relationships?rel=following",
                &following_label,
                current == Relationship::Following,
            ),
            view::Tab::new(
                "/settings/relationships?rel=followers",
                &followers_label,
                current == Relationship::Followers,
            ),
            view::Tab::new(
                "/settings/relationships?rel=requests",
                &received_label,
                current == Relationship::Requests,
            ),
            view::Tab::new(
                "/settings/relationships?rel=sent-requests",
                &sent_label,
                current == Relationship::SentRequests,
            ),
            view::Tab::new(
                "/settings/relationships?rel=mutual",
                &mutual_label,
                current == Relationship::Mutual,
            ),
            view::Tab::new(
                "/settings/relationships?rel=hashtags",
                &hashtags_label,
                current == Relationship::Hashtags,
            ),
            view::Tab::new(
                "/settings/relationships?rel=blocked",
                &blocked_label,
                current == Relationship::Blocked,
            ),
            view::Tab::new(
                "/settings/relationships?rel=muted",
                &muted_label,
                current == Relationship::Muted,
            ),
            view::Tab::new(
                "/settings/relationships?rel=blocked-domains",
                &domains_label,
                current == Relationship::BlockedDomains,
            ),
        ],
    )
}

/// Fetches the account ids for one of the account-backed views, resolves them
/// to account JSON, and computes the "Load more" cursor for that view.
async fn account_rows(
    state: &AppState,
    relationship: Relationship,
    account_id: i64,
    offset: i64,
    max_id: Option<i64>,
) -> Result<(Rows, Option<String>), ApiError> {
    // The follow-derived views page by offset; the block/mute views keyset-page
    // by the last row id, so each computes its own "next" link.
    // These views only ever list the current user's own account, so pass the
    // account as its own viewer: an owner always sees their full lists, member
    // `hide_collections` never applies here.
    let (ids, next_href) = match relationship {
        Relationship::Following => {
            let ids =
                follow::following_page(&state.pool, account_id, offset, LIMIT, Some(account_id))
                    .await?;
            let next = offset_next(relationship, offset, ids.len());
            (ids, next)
        }
        Relationship::Followers => {
            let ids =
                follow::followers_page(&state.pool, account_id, offset, LIMIT, Some(account_id))
                    .await?;
            let next = offset_next(relationship, offset, ids.len());
            (ids, next)
        }
        Relationship::Requests => {
            let entries = follow::requests_of(&state.pool, account_id, max_id, None, LIMIT).await?;
            let next = keyset_next(
                relationship,
                entries.last().map(|e| e.follow_id),
                entries.len(),
            );
            let ids = entries.into_iter().map(|e| e.account_id).collect();
            (ids, next)
        }
        Relationship::SentRequests => {
            let entries =
                follow::sent_requests_of(&state.pool, account_id, max_id, None, LIMIT).await?;
            let next = keyset_next(
                relationship,
                entries.last().map(|e| e.follow_id),
                entries.len(),
            );
            let ids = entries.into_iter().map(|e| e.account_id).collect();
            (ids, next)
        }
        Relationship::Mutual => {
            let ids = follow::mutual_page(&state.pool, account_id, offset, LIMIT).await?;
            let next = offset_next(relationship, offset, ids.len());
            (ids, next)
        }
        Relationship::Blocked => {
            let entries = block::list(&state.pool, account_id, max_id, None, LIMIT).await?;
            let next = keyset_next(
                relationship,
                entries.last().map(|e| e.row_id),
                entries.len(),
            );
            let ids = entries.into_iter().map(|e| e.target_account_id).collect();
            (ids, next)
        }
        Relationship::Muted => {
            let entries = mute::list(&state.pool, account_id, max_id, None, LIMIT).await?;
            let next = keyset_next(
                relationship,
                entries.last().map(|e| e.row_id),
                entries.len(),
            );
            let ids = entries.into_iter().map(|e| e.target_account_id).collect();
            (ids, next)
        }
        // Handled by `domain_rows` / `tag_rows`.
        Relationship::BlockedDomains | Relationship::Hashtags => (Vec::new(), None),
    };

    // One batched render for the whole page instead of a live `account_json`
    // per row; ids whose account row is gone drop out silently, matching the
    // old per-row `find_by_id` skip.
    let accounts =
        render_accounts_by_ids(&state.pool, &state.config.domain, &ids, Some(account_id)).await?;
    Ok((Rows::Accounts(accounts), next_href))
}

/// Fetches the followed hashtags and their "Load more" cursor —
/// keyset-paginated by the follow row id, like the API's `/followed_tags`.
async fn tag_rows(
    state: &AppState,
    relationship: Relationship,
    account_id: i64,
    max_id: Option<i64>,
) -> Result<(Rows, Option<String>), ApiError> {
    let entries = tag::followed(&state.pool, account_id, max_id, None, None, LIMIT).await?;
    let next = keyset_next(
        relationship,
        entries.last().map(|e| e.follow_id),
        entries.len(),
    );
    Ok((Rows::Tags(entries), next))
}

/// Fetches the blocked domains and their "Load more" cursor.
async fn domain_rows(
    state: &AppState,
    relationship: Relationship,
    account_id: i64,
    max_id: Option<i64>,
) -> Result<(Rows, Option<String>), ApiError> {
    let entries = account_domain_block::list(&state.pool, account_id, max_id, None, LIMIT).await?;
    let next = keyset_next(
        relationship,
        entries.last().map(|e| e.row_id),
        entries.len(),
    );
    let domains = entries.into_iter().map(|e| e.domain).collect();
    Ok((Rows::Domains(domains), next))
}

/// The offset-based "Load more" link for a follow-derived view, or `None` when
/// the page wasn't full.
fn offset_next(relationship: Relationship, offset: i64, count: usize) -> Option<String> {
    page_full(count).then(|| {
        format!(
            "/settings/relationships?rel={}&offset={}",
            relationship.slug(),
            offset + LIMIT
        )
    })
}

/// The keyset "Load more" link (carrying the last row id) for a block/mute/
/// domain view, or `None` when the page wasn't full.
fn keyset_next(relationship: Relationship, last_id: Option<i64>, count: usize) -> Option<String> {
    if page_full(count) {
        last_id.map(|id| {
            format!(
                "/settings/relationships?rel={}&max_id={}",
                relationship.slug(),
                id
            )
        })
    } else {
        None
    }
}

/// One selectable account row: a checkbox carrying the account id, beside the
/// standard account card.
fn account_row(value: &serde_json::Value) -> Markup {
    let account = view::Account(value);
    html! {
        li.relationships-list__item {
            label.relationships-list__check {
                input type="checkbox" name="ids" value=(account.id());
            }
            (view::account_card(&account))
        }
    }
}

/// One selectable domain row: a checkbox carrying the domain name, beside the
/// domain shown as plain text.
fn domain_row(domain: &str) -> Markup {
    html! {
        li.relationships-list__item {
            label.relationships-list__check {
                input type="checkbox" name="domains" value=(domain);
            }
            span.relationships-list__domain { (domain) }
        }
    }
}

/// One selectable followed-hashtag row: a checkbox carrying the tag
/// name, beside a link to the hashtag's timeline.
fn tag_row(tag: &tag::FollowedTag) -> Markup {
    html! {
        li.relationships-list__item {
            label.relationships-list__check {
                input type="checkbox" name="tags" value=(tag.name);
            }
            a.relationships-list__domain href=(format!("/tags/{}", tag.name)) {
                "#" (tag.name)
            }
        }
    }
}

/// `POST /web/settings/relationships` — apply the bulk action to the checked
/// account ids (or domains) and return to the same view.
pub async fn bulk_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let action = field(&pairs, "action").unwrap_or_default().to_owned();
    let rel = field(&pairs, "rel").unwrap_or("following").to_owned();

    // The hashtags view submits `tags` (names, not account ids). Unfollowing
    // a tag is purely local, like the API.
    if action == "unfollow_tag" {
        let names: Vec<String> = pairs
            .iter()
            .filter(|(key, _)| key == "tags")
            .map(|(_, value)| value.clone())
            .collect();
        for name in names {
            let found = match tag::find_by_name(&state.pool, &name).await {
                Ok(found) => found,
                Err(err) => return ApiError::from(err).into_response(),
            };
            if let Some(found) = found
                && let Err(err) =
                    tag::unfollow(&state.pool, user.current.account.id, found.id).await
            {
                return ApiError::from(err).into_response();
            }
        }
        return redirect_to(&format!("/settings/relationships?rel={rel}&saved=1"));
    }

    // The domain view submits `domains`; every other view submits account `ids`.
    if action == "unblock_domain" {
        let domains: Vec<String> = pairs
            .iter()
            .filter(|(key, _)| key == "domains")
            .map(|(_, value)| value.clone())
            .collect();
        for domain in domains {
            if let Err(err) = actions::unblock_domain(&state, &user.current.account, &domain).await
            {
                return err.into_response();
            }
        }
        return redirect_to(&format!("/settings/relationships?rel={rel}&saved=1"));
    }

    let mut ids: Vec<i64> = pairs
        .iter()
        .filter(|(key, _)| key == "ids")
        .filter_map(|(_, value)| value.parse().ok())
        .collect();
    // Dedup (a repeated id ran the whole action once per occurrence) and
    // fetch every target in one query; the per-target action below is
    // genuinely per-target (each federates).
    ids.sort_unstable();
    ids.dedup();
    let targets = match account::find_by_ids(&state.pool, &ids).await {
        Ok(targets) => targets,
        Err(err) => return ApiError::from(err).into_response(),
    };

    for target in targets {
        let result = match action.as_str() {
            "remove_follower" => {
                actions::remove_from_followers(&state, &user.current.account, &target).await
            }
            "unblock" => actions::unblock_account(&state, &user.current.account, &target).await,
            "unmute" => actions::unmute_account(&state, &user.current.account, &target).await,
            // Accept/reject a pending follow request. A request already
            // resolved elsewhere (double submit, another tab) surfaces as
            // NotFound from the action; skip it rather than failing the batch.
            "authorize" => {
                match actions::authorize_follow_request(&state, &user.current.account, &target)
                    .await
                {
                    Err(ApiError::NotFound) => Ok(()),
                    other => other,
                }
            }
            "reject" => {
                match actions::reject_follow_request(&state, &user.current.account, &target).await {
                    Err(ApiError::NotFound) => Ok(()),
                    other => other,
                }
            }
            // Default to unfollow; an unknown action tampers with the hidden
            // field, so treat anything else conservatively as unfollow.
            _ => actions::unfollow_account(&state, &user.current.account, &target).await,
        };
        if let Err(err) = result {
            return err.into_response();
        }
    }
    redirect_to(&format!("/settings/relationships?rel={rel}&saved=1"))
}
