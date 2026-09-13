mod account_collections;
pub(crate) mod accounts_api;
pub(crate) mod actors;
mod admin_accounts;
mod admin_instance_policy;
pub(crate) mod admin_metrics;
mod admin_reports;
pub(crate) mod announcements;
pub(crate) mod api;
mod apps;
pub(crate) mod collections_ap;
mod compat;
mod contexts;
mod conversations;
mod directory;
pub(crate) mod domain_blocks;
mod emails;
mod emojis;
mod featured_tags;
mod filters;
mod gateway;
mod hls;
mod hls_live;
mod identity;
mod inbox;
mod instance_meta;
mod lemmy;
mod lists;
mod live_stream;
mod markers;
pub(crate) mod media;
mod notification_filtering;
pub(crate) mod notifications;
pub(crate) mod oauth;
pub(crate) mod params;
mod pleroma_reactions;
mod polls_api;
mod progressive;
mod push;
pub(crate) mod registrations;
mod reports;
pub(crate) mod scheduled_statuses;
pub(crate) mod search;
pub(crate) mod statuses;
mod statuses_api;
mod streaming;
pub(crate) mod suggestions;
pub(crate) mod tags;
mod timelines;
mod trends;
mod virtual_mp4;
mod web_api;
mod webxdc;
mod wellknown;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

use crate::AppState;
use crate::error::ApiError;
use crate::media_processing::{MAX_AV_UPLOAD_BYTES, MAX_UPLOAD_BYTES};

/// Resolve the local actor named by an `ActivityPub` route.
///
/// Legacy actor IDs keep using `/users/{username}`. Accounts created after the
/// immutable-identity migration use `/ap/accounts/{database-id}`; that route is
/// accepted only when it exactly matches the canonical URI stored on the row,
/// so a numeric username can never alias a different actor.
pub(crate) async fn local_actor_account(
    state: &AppState,
    route_key: &str,
    request_uri: &Uri,
) -> Result<plamenu_db::account::Account, ApiError> {
    let account = if request_uri.path().starts_with("/ap/accounts/") {
        let account_id = route_key.parse::<i64>().map_err(|_| ApiError::NotFound)?;
        let account = plamenu_db::account::find_publicly_available_by_id(&state.pool, account_id)
            .await?
            .filter(plamenu_db::account::Account::is_local)
            .ok_or(ApiError::NotFound)?;
        let expected =
            plamenu_db::account::numeric_local_actor_uri(&state.config.domain, account.id);
        if account.uri.as_deref() != Some(expected.as_str()) {
            return Err(ApiError::NotFound);
        }
        account
    } else {
        plamenu_db::account::find_public_local_by_username(&state.pool, route_key)
            .await?
            .ok_or(ApiError::NotFound)?
    };
    Ok(account)
}

/// Rejects requests whose `Accept` header does not ask for an `ActivityPub`
/// representation — the gate on every server-to-server GET endpoint.
fn require_ap_accept(headers: &HeaderMap) -> Result<(), ApiError> {
    if ap_requested(headers) {
        Ok(())
    } else {
        Err(ApiError::NotAcceptable)
    }
}

/// Whether the `Accept` header asks for an `ActivityPub` representation. The
/// actor/status document routes serve the human web page instead when this is
/// false (a browser), like Mastodon's HTML/JSON content negotiation. Also used
/// by the pretty `/@handle` web routes to hand an AP fetch off to the canonical
/// document handler.
pub(crate) fn ap_requested(headers: &HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    plamenu_ap::accepts_activity_json(accept)
}

/// Any origin, no credentials, request headers mirrored back — the policy
/// Mastodon applies to all its CORS routes.
fn cors_any_origin() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(AllowHeaders::mirror_request())
}

/// Notifications — the v1 flat listing, the v2 grouped listing — and the
/// read-state machinery around them (markers).
fn notification_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/notifications", get(notifications::index))
        .route(
            "/api/v1/notifications/unread_count",
            get(notifications::unread_count),
        )
        .route("/api/v1/notifications/clear", post(notifications::clear))
        .route(
            "/api/v1/notifications/policy",
            get(notification_filtering::policy_v1)
                .patch(notification_filtering::update_policy_v1)
                .put(notification_filtering::update_policy_v1),
        )
        .route(
            "/api/v1/notifications/requests",
            get(notification_filtering::requests_index),
        )
        .route(
            "/api/v1/notifications/requests/merged",
            get(notification_filtering::requests_merged),
        )
        .route(
            "/api/v1/notifications/requests/accept",
            post(notification_filtering::accept_requests),
        )
        .route(
            "/api/v1/notifications/requests/dismiss",
            post(notification_filtering::dismiss_requests),
        )
        .route(
            "/api/v1/notifications/requests/{id}",
            get(notification_filtering::request_show),
        )
        .route(
            "/api/v1/notifications/requests/{id}/accept",
            post(notification_filtering::accept_request),
        )
        .route(
            "/api/v1/notifications/requests/{id}/dismiss",
            post(notification_filtering::dismiss_request),
        )
        .route("/api/v1/notifications/{id}", get(notifications::show))
        .route(
            "/api/v1/notifications/{id}/dismiss",
            post(notifications::dismiss),
        )
        .route("/api/v2/notifications", get(notifications::index_v2))
        .route(
            "/api/v2/notifications/policy",
            get(notification_filtering::policy_v2)
                .patch(notification_filtering::update_policy_v2)
                .put(notification_filtering::update_policy_v2),
        )
        .route(
            "/api/v2/notifications/unread_count",
            get(notifications::unread_count_v2),
        )
        .route("/api/v2/notifications/clear", post(notifications::clear))
        .route(
            "/api/v2/notifications/{group_key}",
            get(notifications::show_v2),
        )
        .route(
            "/api/v2/notifications/{group_key}/dismiss",
            post(notifications::dismiss_v2),
        )
        .route(
            "/api/v2/notifications/{group_key}/accounts",
            get(notifications::group_accounts_v2),
        )
        .route("/api/v1/markers", get(markers::index).post(markers::create))
}

/// The `/api/v1/accounts/*` routes plus the social-graph listings that hang
/// off them (blocks, mutes).
#[allow(clippy::too_many_lines, reason = "flat account route registry")]
fn account_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/accounts/verify_credentials",
            get(accounts_api::verify_credentials),
        )
        .route("/api/v1/accounts/lookup", get(accounts_api::lookup))
        .route("/api/v1/accounts/search", get(search::search_accounts_v1))
        .route(
            "/api/v1/accounts/relationships",
            get(accounts_api::relationships),
        )
        .route(
            "/api/v1/accounts/familiar_followers",
            get(accounts_api::familiar_followers),
        )
        .route(
            "/api/v1/accounts",
            get(accounts_api::index).post(registrations::create),
        )
        .route("/invite/{code}", get(registrations::show_invite))
        .route(
            "/api/v1/accounts/{id}/email_subscriptions",
            post(emails::email_subscriptions),
        )
        .route("/api/v1/accounts/{id}", get(accounts_api::show))
        .route(
            "/api/v1/accounts/{id}/identity_statements",
            get(identity::list),
        )
        .route(
            "/api/v1/accounts/identity_statements",
            post(identity::publish)
                .delete(identity::remove)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/api/v1/accounts/{id}/remote_history",
            get(accounts_api::remote_history_show),
        )
        .route(
            "/api/v1/accounts/{id}/remote_history/fetch",
            post(accounts_api::remote_history_fetch),
        )
        .route(
            "/api/v1/accounts/{id}/statuses",
            get(accounts_api::statuses),
        )
        .route(
            "/api/v1/accounts/{id}/followers",
            get(accounts_api::followers),
        )
        .route(
            "/api/v1/accounts/{id}/following",
            get(accounts_api::following),
        )
        .route("/api/v1/accounts/{id}/follow", post(accounts_api::follow))
        .route(
            "/api/v1/accounts/{id}/unfollow",
            post(accounts_api::unfollow),
        )
        .route("/api/v1/accounts/{id}/block", post(accounts_api::block))
        .route("/api/v1/accounts/{id}/unblock", post(accounts_api::unblock))
        .route("/api/v1/accounts/{id}/mute", post(accounts_api::mute))
        .route("/api/v1/accounts/{id}/unmute", post(accounts_api::unmute))
        .route("/api/v1/accounts/{id}/note", post(accounts_api::note))
        .route(
            "/api/v1/accounts/{id}/remove_from_followers",
            post(accounts_api::remove_from_followers),
        )
        .route("/api/v1/accounts/{id}/endorse", post(accounts_api::endorse))
        .route(
            "/api/v1/accounts/{id}/unendorse",
            post(accounts_api::unendorse),
        )
        // Mastodon's deprecated `/pin` and `/unpin` aliases for endorse.
        .route("/api/v1/accounts/{id}/pin", post(accounts_api::endorse))
        .route("/api/v1/accounts/{id}/unpin", post(accounts_api::unendorse))
        .route(
            "/api/v1/accounts/{id}/endorsements",
            get(accounts_api::account_endorsements),
        )
        .route(
            "/api/v1/accounts/{id}/featured_tags",
            get(featured_tags::account_index),
        )
        .route(
            "/api/v1/endorsements",
            get(accounts_api::endorsements_index),
        )
        .route("/api/v1/blocks", get(accounts_api::blocks_index))
        .route("/api/v1/mutes", get(accounts_api::mutes_index))
        .route(
            "/api/v1/domain_blocks",
            get(domain_blocks::index)
                .post(domain_blocks::create)
                .delete(domain_blocks::destroy),
        )
        .route("/api/v1/domain_blocks/preview", get(domain_blocks::preview))
        .route(
            "/api/v1/follow_requests",
            get(accounts_api::follow_requests_index),
        )
        .route(
            "/api/v1/follow_requests/{id}/authorize",
            post(accounts_api::follow_requests_authorize),
        )
        .route(
            "/api/v1/follow_requests/{id}/reject",
            post(accounts_api::follow_requests_reject),
        )
        .route("/api/v1/accounts/{id}/lists", get(lists::account_lists))
        .route(
            "/api/v1/accounts/{id}/collections",
            get(account_collections::index),
        )
        .route(
            "/api/v1/accounts/{id}/in_collections",
            get(account_collections::in_collections),
        )
}

/// Account collections (Mastodon 4.6 / FEP-7aa9): CRUD and membership.
fn collection_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/collections", post(account_collections::create))
        .route(
            "/api/v1/collections/{id}",
            get(account_collections::show)
                .patch(account_collections::update)
                .put(account_collections::update)
                .delete(account_collections::destroy),
        )
        .route(
            "/api/v1/collections/{id}/items",
            post(account_collections::add_item),
        )
        .route(
            "/api/v1/collections/{id}/items/{item_id}",
            axum::routing::delete(account_collections::remove_item),
        )
        .route(
            "/api/v1/collections/{id}/items/{item_id}/revoke",
            post(account_collections::revoke_item),
        )
}

/// Statuses: CRUD, the interaction verbs (favourite/reblog/bookmark/pin/mute
/// of a conversation), the actor listings, and polls.
fn status_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/pleroma/statuses/{id}/reactions",
            get(pleroma_reactions::index),
        )
        .route(
            "/api/v1/pleroma/statuses/{id}/reactions/{emoji}",
            get(pleroma_reactions::index_emoji)
                .put(pleroma_reactions::create)
                .delete(pleroma_reactions::delete),
        )
        .route(
            "/api/v1/statuses",
            get(statuses_api::index).post(statuses_api::create),
        )
        // Static sibling — wins over `/{id}` at the same position (matchit).
        .route("/api/v1/statuses/preview", post(statuses_api::preview))
        .route(
            "/api/v1/statuses/{id}",
            get(statuses_api::show)
                .put(statuses_api::update)
                .patch(statuses_api::update)
                .delete(statuses_api::delete),
        )
        .route("/api/v1/statuses/{id}/context", get(statuses_api::context))
        .route("/api/v1/statuses/{id}/quotes", get(statuses_api::quotes))
        .route(
            "/api/v1/statuses/{id}/quotes/{quote_id}/revoke",
            post(statuses_api::revoke_quote),
        )
        .route(
            "/api/v1/statuses/{id}/interaction_policy",
            patch(statuses_api::interaction_policy).put(statuses_api::interaction_policy),
        )
        .route("/api/v1/statuses/{id}/history", get(statuses_api::history))
        .route("/api/v1/statuses/{id}/source", get(statuses_api::source))
        .route(
            "/api/v1/statuses/{id}/translate",
            post(statuses_api::translate),
        )
        .route(
            "/api/v1/statuses/{id}/favourite",
            post(statuses_api::favourite),
        )
        .route(
            "/api/v1/statuses/{id}/unfavourite",
            post(statuses_api::unfavourite),
        )
        // Plamenu extension: downvotes on group posts. The
        // upvote verb is `favourite`.
        .route(
            "/api/v1/statuses/{id}/downvote",
            post(statuses_api::downvote),
        )
        .route(
            "/api/v1/statuses/{id}/undownvote",
            post(statuses_api::undownvote),
        )
        .route("/api/v1/statuses/{id}/reblog", post(statuses_api::reblog))
        .route(
            "/api/v1/statuses/{id}/unreblog",
            post(statuses_api::unreblog),
        )
        .route(
            "/api/v1/statuses/{id}/bookmark",
            post(statuses_api::bookmark),
        )
        .route(
            "/api/v1/statuses/{id}/unbookmark",
            post(statuses_api::unbookmark),
        )
        .route("/api/v1/statuses/{id}/pin", post(statuses_api::pin))
        .route("/api/v1/statuses/{id}/unpin", post(statuses_api::unpin))
        .route("/api/v1/statuses/{id}/mute", post(statuses_api::mute))
        .route("/api/v1/statuses/{id}/unmute", post(statuses_api::unmute))
        .route(
            "/api/v1/statuses/{id}/favourited_by",
            get(statuses_api::favourited_by),
        )
        .route(
            "/api/v1/statuses/{id}/reblogged_by",
            get(statuses_api::reblogged_by),
        )
        .route("/api/v1/bookmarks", get(statuses_api::bookmarks_index))
        .route("/api/v1/favourites", get(statuses_api::favourites_index))
        .route("/api/v1/scheduled_statuses", get(scheduled_statuses::index))
        .route(
            "/api/v1/scheduled_statuses/{id}",
            get(scheduled_statuses::show)
                .put(scheduled_statuses::update)
                .patch(scheduled_statuses::update)
                .delete(scheduled_statuses::destroy),
        )
        .route("/api/v1/polls/{id}", get(polls_api::show))
        .route("/api/v1/polls/{id}/votes", post(polls_api::vote))
        .merge(event_routes())
}

/// Event participation (the E-track): RSVPs and the organizer's attendee list.
/// Wholly a Plamenu extension — Mastodon has no event API at all, so there is
/// nothing here to be compatible with.
fn event_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/statuses/{id}/participate",
            post(statuses_api::participate),
        )
        .route(
            "/api/v1/statuses/{id}/unparticipate",
            post(statuses_api::unparticipate),
        )
        // The attendee list is organizer-only — a guest list is not public
        // information, and no ecosystem convention says otherwise.
        .route(
            "/api/v1/statuses/{id}/participants",
            get(statuses_api::participants),
        )
        .route(
            "/api/v1/statuses/{id}/participants/{account_id}/approve",
            post(statuses_api::approve_participant),
        )
        .route(
            "/api/v1/statuses/{id}/participants/{account_id}/reject",
            post(statuses_api::reject_participant),
        )
}

/// Direct-message conversations: listing, read-state toggles and deletion.
fn conversation_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/conversations", get(conversations::index))
        .route(
            "/api/v1/conversations/{id}",
            axum::routing::delete(conversations::destroy),
        )
        .route("/api/v1/conversations/{id}/read", post(conversations::read))
        .route(
            "/api/v1/conversations/{id}/unread",
            post(conversations::unread),
        )
}

/// Server announcements: the published listing, per-user dismiss, and emoji
/// reactions.
fn announcement_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/announcements", get(announcements::index))
        .route(
            "/api/v1/announcements/{id}/dismiss",
            post(announcements::dismiss),
        )
        // Rails' resourceful `update` answers both PUT and PATCH.
        .route(
            "/api/v1/announcements/{id}/reactions/{name}",
            put(announcements::react)
                .patch(announcements::react)
                .delete(announcements::unreact),
        )
}

/// User lists: CRUD, membership management and the list timeline.
fn list_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/lists", get(lists::index).post(lists::create))
        .route(
            "/api/v1/lists/{id}",
            get(lists::show)
                .put(lists::update)
                .patch(lists::update)
                .delete(lists::destroy),
        )
        .route(
            "/api/v1/lists/{id}/accounts",
            get(lists::accounts_index)
                .post(lists::accounts_add)
                .delete(lists::accounts_remove),
        )
        .route("/api/v1/timelines/list/{id}", get(lists::timeline))
}

/// The `/api/*` client routes: everything CORS-allowed for any origin, with
/// the pagination/rate-limit headers clients read exposed.
/// Timelines and their live counterpart, the streaming websocket.
fn timeline_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/streaming", get(streaming::streaming))
        .route("/api/v1/streaming/health", get(streaming::health))
        .route("/api/v1/timelines/home", get(timelines::home))
        .route("/api/v1/timelines/public", get(timelines::public))
        .route("/api/v1/timelines/tag/{hashtag}", get(timelines::tag))
}

/// Content filters: the v2 filter-with-rules model (filters, their keywords
/// and pinned statuses) and the deprecated v1 single-keyword API.
fn filter_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v2/filters",
            get(filters::index_v2).post(filters::create_v2),
        )
        .route(
            "/api/v2/filters/keywords/{id}",
            get(filters::keyword_show)
                .put(filters::keyword_update)
                .patch(filters::keyword_update)
                .delete(filters::keyword_destroy),
        )
        .route(
            "/api/v2/filters/statuses/{id}",
            get(filters::status_show).delete(filters::status_destroy),
        )
        .route(
            "/api/v2/filters/{filter_id}/keywords",
            get(filters::keywords_index).post(filters::keyword_create),
        )
        .route(
            "/api/v2/filters/{filter_id}/statuses",
            get(filters::statuses_index).post(filters::status_create),
        )
        .route(
            "/api/v2/filters/{id}",
            get(filters::show_v2)
                .put(filters::update_v2)
                .patch(filters::update_v2)
                .delete(filters::destroy_v2),
        )
        .route(
            "/api/v1/filters",
            get(filters::index_v1).post(filters::create_v1),
        )
        .route(
            "/api/v1/filters/{id}",
            get(filters::show_v1)
                .put(filters::update_v1)
                .patch(filters::update_v1)
                .delete(filters::destroy_v1),
        )
}

/// Web Push registration — Mastodon's singular `resource :subscription`.
fn push_routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/push/subscription",
        post(push::create)
            .get(push::show)
            .put(push::update)
            .patch(push::update)
            .delete(push::destroy),
    )
}

/// Mastodon's browser-session API used by the React web client.
fn web_client_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/web/settings",
            patch(web_api::settings).put(web_api::settings),
        )
        .route("/api/web/embeds/{id}", get(web_api::embed))
        .route("/api/web/push_subscriptions", post(web_api::push_create))
        .route(
            "/api/web/push_subscriptions/{id}",
            patch(web_api::push_update)
                .put(web_api::push_update)
                .delete(web_api::push_destroy),
        )
}

fn media_routes() -> Router<AppState> {
    // Media uploads carry video files (Mastodon's 99 MB limit); profile
    // images stay at the image limit.
    Router::new()
        .route("/api/v1/media", post(media::upload_v1))
        .route("/api/v2/media", post(media::upload_v2))
        .layer(DefaultBodyLimit::max(MAX_AV_UPLOAD_BYTES + 1024 * 1024))
        .route(
            "/api/v1/media/{id}",
            get(media::show)
                .put(media::update)
                .patch(media::update)
                .delete(media::destroy),
        )
}

/// The `/api/v1|v2/admin/*` moderation surface. Every handler gates itself via
/// the [`AdminUser`](crate::auth::AdminUser) extractor (role bit + broad or
/// resource-specific `admin:*` scope).
fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/admin/accounts", get(admin_accounts::index_v1))
        .route("/api/v2/admin/accounts", get(admin_accounts::index_v2))
        .route(
            "/api/v1/admin/accounts/{id}",
            get(admin_accounts::show).delete(admin_accounts::destroy),
        )
        .route(
            "/api/v1/admin/accounts/{id}/action",
            post(admin_accounts::create_action),
        )
        .route(
            "/api/v1/admin/accounts/{id}/approve",
            post(admin_accounts::approve),
        )
        .route(
            "/api/v1/admin/accounts/{id}/reject",
            post(admin_accounts::reject),
        )
        .route(
            "/api/v1/admin/accounts/{id}/enable",
            post(admin_accounts::enable),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsuspend",
            post(admin_accounts::unsuspend),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsilence",
            post(admin_accounts::unsilence),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsensitive",
            post(admin_accounts::unsensitive),
        )
        .route("/api/v1/admin/reports", get(admin_reports::index))
        .route(
            "/api/v1/admin/reports/{id}",
            get(admin_reports::show)
                .put(admin_reports::update)
                .patch(admin_reports::update),
        )
        .route(
            "/api/v1/admin/reports/{id}/assign_to_self",
            post(admin_reports::assign_to_self),
        )
        .route(
            "/api/v1/admin/reports/{id}/unassign",
            post(admin_reports::unassign),
        )
        .route(
            "/api/v1/admin/reports/{id}/reopen",
            post(admin_reports::reopen),
        )
        .route(
            "/api/v1/admin/reports/{id}/resolve",
            post(admin_reports::resolve),
        )
        .route("/api/v1/admin/measures", post(admin_metrics::measures))
        .route("/api/v1/admin/dimensions", post(admin_metrics::dimensions))
        .route("/api/v1/admin/retention", post(admin_metrics::retention))
        .merge(admin_trends_routes())
        .merge(admin_instance_policy_routes())
}

/// The public discovery surface: `/api/v1/trends/{tags,statuses,links}` (with
/// the deprecated `/api/v1/trends` alias), the `/api/v1/timelines/link` feed,
/// and follow suggestions, split out of [`api_routes`] to keep each builder
/// readable.
fn trends_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/trends", get(trends::tags_index))
        .route("/api/v1/trends/tags", get(trends::tags_index))
        .route("/api/v1/trends/statuses", get(trends::statuses_index))
        .route("/api/v1/trends/links", get(trends::links_index))
        .route("/api/v1/timelines/link", get(trends::link_timeline))
        .route("/api/v1/suggestions", get(suggestions::index_v1))
        .route("/api/v1/suggestions/{id}", delete(suggestions::destroy))
        .route("/api/v2/suggestions", get(suggestions::index_v2))
}

/// The `/api/v1/admin/tags` registry and `/api/v1/admin/trends/*` review
/// surface (tags, statuses, links + link publishers), split out of
/// [`admin_routes`] to keep each builder readable.
fn admin_trends_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/admin/tags", get(trends::admin_tags_index))
        .route(
            "/api/v1/admin/tags/{id}",
            get(trends::admin_tags_show)
                .put(trends::admin_tags_update)
                .patch(trends::admin_tags_update),
        )
        .route("/api/v1/admin/trends/tags", get(trends::admin_trends_index))
        .route(
            "/api/v1/admin/trends/tags/{id}/approve",
            post(trends::admin_trends_approve),
        )
        .route(
            "/api/v1/admin/trends/tags/{id}/reject",
            post(trends::admin_trends_reject),
        )
        .route(
            "/api/v1/admin/trends/statuses",
            get(trends::admin_trends_statuses_index),
        )
        .route(
            "/api/v1/admin/trends/statuses/{id}/approve",
            post(trends::admin_trends_statuses_approve),
        )
        .route(
            "/api/v1/admin/trends/statuses/{id}/reject",
            post(trends::admin_trends_statuses_reject),
        )
        .route(
            "/api/v1/admin/trends/links",
            get(trends::admin_trends_links_index),
        )
        .route(
            "/api/v1/admin/trends/links/{id}/approve",
            post(trends::admin_trends_links_approve),
        )
        .route(
            "/api/v1/admin/trends/links/{id}/reject",
            post(trends::admin_trends_links_reject),
        )
        .route(
            "/api/v1/admin/trends/links/publishers",
            get(trends::admin_link_publishers_index),
        )
        .route(
            "/api/v1/admin/trends/links/publishers/{id}/approve",
            post(trends::admin_link_publishers_approve),
        )
        .route(
            "/api/v1/admin/trends/links/publishers/{id}/reject",
            post(trends::admin_link_publishers_reject),
        )
}

/// The `/api/v1/admin/{domain,email,ip,canonical_email}_*` instance-policy
/// surface, split out of [`admin_routes`] to keep each builder readable.
fn admin_instance_policy_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/admin/domain_blocks",
            get(admin_instance_policy::domain_blocks_index)
                .post(admin_instance_policy::domain_blocks_create),
        )
        .route(
            "/api/v1/admin/domain_blocks/{id}",
            get(admin_instance_policy::domain_blocks_show)
                .patch(admin_instance_policy::domain_blocks_update)
                .put(admin_instance_policy::domain_blocks_update)
                .delete(admin_instance_policy::domain_blocks_destroy),
        )
        .route(
            "/api/v1/admin/domain_allows",
            get(admin_instance_policy::domain_allows_index)
                .post(admin_instance_policy::domain_allows_create),
        )
        .route(
            "/api/v1/admin/domain_allows/{id}",
            get(admin_instance_policy::domain_allows_show)
                .delete(admin_instance_policy::domain_allows_destroy),
        )
        .route(
            "/api/v1/admin/email_domain_blocks",
            get(admin_instance_policy::email_domain_blocks_index)
                .post(admin_instance_policy::email_domain_blocks_create),
        )
        .route(
            "/api/v1/admin/email_domain_blocks/{id}",
            get(admin_instance_policy::email_domain_blocks_show)
                .delete(admin_instance_policy::email_domain_blocks_destroy),
        )
        .route(
            "/api/v1/admin/ip_blocks",
            get(admin_instance_policy::ip_blocks_index)
                .post(admin_instance_policy::ip_blocks_create),
        )
        .route(
            "/api/v1/admin/ip_blocks/{id}",
            get(admin_instance_policy::ip_blocks_show)
                .patch(admin_instance_policy::ip_blocks_update)
                .put(admin_instance_policy::ip_blocks_update)
                .delete(admin_instance_policy::ip_blocks_destroy),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks",
            get(admin_instance_policy::canonical_email_blocks_index)
                .post(admin_instance_policy::canonical_email_blocks_create),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks/test",
            post(admin_instance_policy::canonical_email_blocks_test),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks/{id}",
            get(admin_instance_policy::canonical_email_blocks_show)
                .delete(admin_instance_policy::canonical_email_blocks_destroy),
        )
}

/// `/api/v1/emails/*` — the confirmation endpoints for unconfirmed sign-ups.
fn email_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/emails/confirmations",
            post(emails::resend_confirmation),
        )
        .route(
            "/api/v1/emails/check_confirmation",
            get(emails::check_confirmation),
        )
}

/// The public discovery surface: instance meta, `/api/oembed`
/// and the profile directory.
fn instance_meta_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/directory", get(directory::directory))
        .route("/api/v1/instance/peers", get(instance_meta::peers))
        .route("/api/v1/instance/activity", get(instance_meta::activity))
        .route(
            "/api/v1/instance/domain_blocks",
            get(instance_meta::domain_blocks),
        )
        .route("/api/v1/instance/languages", get(instance_meta::languages))
        .route(
            "/api/v1/instance/translation_languages",
            get(instance_meta::translation_languages),
        )
        .route("/api/v1/peers/search", get(instance_meta::peers_search))
        .route("/api/oembed", get(instance_meta::oembed))
}

#[allow(clippy::too_many_lines, reason = "one flat route table")]
fn api_routes(state: AppState) -> Router<AppState> {
    let profile_routes = Router::new()
        .route(
            "/api/v1/accounts/update_credentials",
            patch(accounts_api::update_credentials),
        )
        .route(
            "/api/v1/profile",
            get(accounts_api::profile_show)
                .patch(accounts_api::profile_update)
                // Mastodon routes this as a Rails `resource :profile, :update`,
                // which answers PUT as well as PATCH — and masto.js (Phanpy,
                // Elk) sends PUT for every `update` action. Every other route
                // Mastodon declares `:update` on is paired the same way.
                .put(accounts_api::profile_update),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 1024 * 1024))
        .route(
            "/api/v1/profile/avatar",
            delete(accounts_api::profile_delete_avatar),
        )
        .route(
            "/api/v1/profile/header",
            delete(accounts_api::profile_delete_header),
        );

    Router::new()
        .merge(media_routes())
        .merge(profile_routes)
        .merge(account_routes())
        .merge(timeline_routes())
        .merge(list_routes())
        .merge(collection_routes())
        .merge(filter_routes())
        .merge(admin_routes())
        .merge(email_routes())
        .merge(lemmy::router(state))
        .route("/api/v1/apps", post(apps::create))
        .route(
            "/api/v1/apps/verify_credentials",
            get(apps::verify_credentials),
        )
        .route("/api/v1/custom_emojis", get(compat::custom_emojis))
        .merge(announcement_routes())
        .route("/api/v1/followed_tags", get(tags::followed_index))
        .route("/api/v1/tags/{id}", get(tags::show))
        .route("/api/v1/tags/{id}/follow", post(tags::follow))
        .route("/api/v1/tags/{id}/unfollow", post(tags::unfollow))
        .route("/api/v1/tags/{id}/feature", post(tags::feature))
        .route("/api/v1/tags/{id}/unfeature", post(tags::unfeature))
        .route(
            "/api/v1/featured_tags",
            get(featured_tags::index).post(featured_tags::create),
        )
        .route(
            "/api/v1/featured_tags/suggestions",
            get(featured_tags::suggestions),
        )
        .route("/api/v1/featured_tags/{id}", delete(featured_tags::destroy))
        .route("/api/v1/preferences", get(compat::preferences))
        .merge(trends_routes())
        .merge(status_routes())
        .merge(notification_routes())
        .merge(push_routes())
        .merge(web_client_routes())
        .merge(conversation_routes())
        .route("/api/v1/reports", post(reports::create))
        .route("/api/v1/instance", get(api::instance_v1))
        // Trailing-slash aliases: Mastodon.py (through at least 2.2.1) calls
        // `/api/v1/instance/`, `/api/v2/instance/`, `/api/v1/reports/` and
        // `/api/v1/admin/domain_blocks/`; Rails treats them as the same route,
        // axum does not. Kept to exactly the set that library emits.
        .route("/api/v1/instance/", get(api::instance_v1))
        .route("/api/v2/instance/", get(api::instance_v2))
        .route("/api/v1/reports/", post(reports::create))
        .route(
            "/api/v1/admin/domain_blocks/",
            get(admin_instance_policy::domain_blocks_index)
                .post(admin_instance_policy::domain_blocks_create),
        )
        .route("/api/v1/instance/rules", get(api::instance_rules))
        .route(
            "/api/v1/instance/extended_description",
            get(api::instance_extended_description),
        )
        .route(
            "/api/v1/instance/privacy_policy",
            get(api::instance_privacy_policy),
        )
        .route(
            "/api/v1/instance/terms_of_service",
            get(api::instance_terms_of_service),
        )
        .route(
            "/api/v1/instance/terms_of_service/{date}",
            get(api::instance_terms_of_service),
        )
        .merge(instance_meta_routes())
        .route("/api/v2/instance", get(api::instance_v2))
        .route("/api/v2/search", get(search::search_v2))
        .layer(
            cors_any_origin()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .expose_headers([
                    header::LINK,
                    HeaderName::from_static("x-ratelimit-reset"),
                    HeaderName::from_static("x-ratelimit-limit"),
                    HeaderName::from_static("x-ratelimit-remaining"),
                    HeaderName::from_static("x-request-id"),
                ]),
        )
}

/// How long the readiness probe waits for a database round-trip before it
/// reports the instance unready. Kept under the container health-check timeout
/// so a stuck or exhausted pool surfaces as an explicit `503`, not a probe
/// timeout that reads as a network fault.
const READINESS_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// `POST /csp-report` — the sink for the enforced content-security policy's
/// `report-uri` (`crate::security_headers`). Browsers send an
/// `application/csp-report` JSON body; the interesting fields are logged so
/// violations can be audited from the journal. The body is attacker-postable
/// (unauthenticated by nature — browsers attach no credentials), so it is
/// bounded by the route's body limit, each logged field is truncated, and the
/// answer is always `204`: a malformed report is a misbehaving client, not an
/// error of ours.
async fn csp_report(body: axum::body::Bytes) -> StatusCode {
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return StatusCode::NO_CONTENT;
    };
    // Reports arrive wrapped in a `csp-report` object (report-uri's shape);
    // tolerate a bare object for Reporting-API-shaped senders.
    let detail = report.get("csp-report").unwrap_or(&report);
    let field = |name: &str| -> String {
        detail
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect()
    };
    tracing::warn!(
        document_uri = %field("document-uri"),
        violated_directive = %field("violated-directive"),
        blocked_uri = %field("blocked-uri"),
        source_file = %field("source-file"),
        "csp violation reported"
    );
    StatusCode::NO_CONTENT
}

/// The report sink does not exist unless the operator deliberately opts into
/// CSP telemetry. Keeping the route itself behind the flag makes the disabled
/// state externally indistinguishable from a Plamenu build without reporting.
fn csp_report_routes(enabled: bool) -> Router<AppState> {
    if enabled {
        Router::new().route(
            "/csp-report",
            post(csp_report).layer(DefaultBodyLimit::max(16 * 1024)),
        )
    } else {
        Router::new()
    }
}

/// Liveness (`GET /health`): the process is up and the async runtime is
/// scheduling. Deliberately cheap and dependency-free — it stays `200` even
/// while the database or a background worker is unavailable, so an orchestrator
/// can tell "process alive, do not restart" apart from "not able to serve"
/// ([`readiness`]). Restarting on a transient database blip would only
/// compound an outage.
async fn liveness() -> &'static str {
    "OK"
}

/// Readiness (`GET /ready`): whether the instance can actually serve requests
/// that need its datastore. Runs a bounded connectivity query against the
/// pool; a dead, unreachable, or exhausted-pool database answers `503` instead
/// of the constant `200` a bare liveness probe returns, so container health,
/// dependency gating, and monitoring reflect real serve-ability. It also
/// reflects supervisor state: a background worker that is
/// restart-looping — crashing as fast as its supervisor respawns it — turns
/// the instance unready, with the failing subsystems named in the body. An
/// isolated worker crash that recovers on the next respawn does not.
async fn readiness(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(READINESS_DB_TIMEOUT, plamenu_db::ping(&state.pool)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "readiness probe: database query failed");
            return (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response();
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = READINESS_DB_TIMEOUT.as_secs(),
                "readiness probe: database query timed out"
            );
            return (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response();
        }
    }
    let looping = state.workers.restart_looping();
    if !looping.is_empty() {
        tracing::warn!(workers = ?looping, "readiness probe: workers restart-looping");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: workers restart-looping: {}", looping.join(", ")),
        )
            .into_response();
    }
    (StatusCode::OK, "ready").into_response()
}

#[allow(clippy::too_many_lines, reason = "one flat route table")]
pub fn router(state: AppState) -> Router {
    // In authorized-fetch mode every AP object GET requires a signature;
    // discovery (webfinger, nodeinfo) and the instance actor stay public so
    // secure-mode servers can bootstrap each other's keys.
    let signed_fetch_gate =
        axum::middleware::from_fn_with_state(state.clone(), crate::signed_fetch::gate);

    // CORS grouping mirrors Mastodon: discovery endpoints are GET-only,
    // /api/* allows everything, the oauth token endpoints are POST-only.
    // Federation endpoints (inboxes, AP objects) are server-to-server and
    // get no CORS.
    let discovery_routes = Router::new()
        .route("/.well-known/host-meta", get(wellknown::host_meta))
        .route(
            "/.well-known/host-meta.json",
            get(wellknown::host_meta_json),
        )
        .route("/.well-known/webfinger", get(wellknown::webfinger))
        .route(
            "/.well-known/oauth-authorization-server",
            get(wellknown::oauth_metadata),
        )
        .route("/.well-known/nodeinfo", get(wellknown::nodeinfo_index))
        .route("/nodeinfo/2.0", get(wellknown::nodeinfo_20))
        .route("/nodeinfo/2.1", get(wellknown::nodeinfo_21))
        .route("/actor", get(crate::instance_actor::get_actor))
        .layer(cors_any_origin().allow_methods([Method::GET]));

    // The user actor document keeps discovery CORS but is NOT wrapped in the
    // shared gate: under secure mode `actors::get_actor` owns the policy itself
    // so it can serve a key-only "blanked" actor to unsigned callers (letting
    // peers that dereference us unsigned still verify our signatures) while a
    // valid signature unlocks the full document. The full profile, statuses,
    // outbox and collections stay gated via `federation_object_routes` below.
    let actor_route = Router::new()
        .route("/users/{username}", get(actors::get_actor))
        .route("/ap/accounts/{username}", get(actors::get_actor))
        .layer(cors_any_origin().allow_methods([Method::GET]));

    let federation_object_routes = Router::new()
        .route("/users/{username}/outbox", get(actors::get_outbox))
        .route("/ap/accounts/{username}/outbox", get(actors::get_outbox))
        .route("/users/{username}/followers", get(actors::get_followers))
        .route(
            "/ap/accounts/{username}/followers",
            get(actors::get_followers),
        )
        .route(
            "/users/{username}/followers_synchronization",
            get(actors::get_followers_synchronization),
        )
        .route(
            "/ap/accounts/{username}/followers_synchronization",
            get(actors::get_followers_synchronization),
        )
        .route("/users/{username}/following", get(actors::get_following))
        .route(
            "/ap/accounts/{username}/following",
            get(actors::get_following),
        )
        .route(
            "/users/{username}/collections/featured",
            get(actors::get_featured),
        )
        .route(
            "/ap/accounts/{username}/collections/featured",
            get(actors::get_featured),
        )
        .route(
            "/users/{username}/collections/tags",
            get(actors::get_featured_tags),
        )
        .route(
            "/ap/accounts/{username}/collections/tags",
            get(actors::get_featured_tags),
        )
        .route(
            "/users/{username}/featured_collections",
            get(collections_ap::get_featured_collections),
        )
        .route(
            "/ap/accounts/{username}/featured_collections",
            get(collections_ap::get_featured_collections),
        )
        .route("/users/{username}/moderators", get(actors::get_moderators))
        .route(
            "/ap/accounts/{username}/moderators",
            get(actors::get_moderators),
        )
        .route(
            "/users/{username}/affiliations",
            get(actors::get_affiliations),
        )
        .route(
            "/ap/accounts/{username}/affiliations",
            get(actors::get_affiliations),
        )
        .route(
            "/users/{username}/collections/{id}",
            get(collections_ap::get_collection),
        )
        .route(
            "/ap/accounts/{username}/collections/{id}",
            get(collections_ap::get_collection),
        )
        .route(
            "/users/{username}/feature_authorizations/{id}",
            get(collections_ap::get_feature_authorization),
        )
        .route(
            "/ap/accounts/{username}/feature_authorizations/{id}",
            get(collections_ap::get_feature_authorization),
        )
        .route("/contexts/{id}", get(contexts::get_context))
        .route("/contexts/{id}/history", get(contexts::get_context_history))
        .route("/users/{username}/statuses/{id}", get(statuses::get_status))
        .route(
            "/ap/accounts/{username}/statuses/{id}",
            get(statuses::get_status),
        )
        .route(
            "/users/{username}/statuses/{id}/replies",
            get(statuses::get_replies),
        )
        .route(
            "/ap/accounts/{username}/statuses/{id}/replies",
            get(statuses::get_replies),
        )
        .route(
            "/users/{username}/statuses/{id}/likes",
            get(statuses::get_likes),
        )
        .route(
            "/ap/accounts/{username}/statuses/{id}/likes",
            get(statuses::get_likes),
        )
        .route(
            "/users/{username}/statuses/{id}/shares",
            get(statuses::get_shares),
        )
        .route(
            "/ap/accounts/{username}/statuses/{id}/shares",
            get(statuses::get_shares),
        )
        .route(
            "/users/{username}/quote_authorizations/{id}",
            get(statuses::get_quote_authorization),
        )
        .route(
            "/ap/accounts/{username}/quote_authorizations/{id}",
            get(statuses::get_quote_authorization),
        )
        .route("/emojis/{id}", get(emojis::get_emoji))
        .route_layer(signed_fetch_gate);

    let oauth_token_routes = Router::new()
        .route("/oauth/token", post(oauth::token))
        .route("/oauth/revoke", post(oauth::revoke))
        .layer(cors_any_origin().allow_methods([Method::POST]));

    // Media is read cross-origin by web clients: Phanpy loads avatars with
    // `crossOrigin="anonymous"` to sample their alpha channel on a canvas, and
    // any client drawing media to a canvas needs `Access-Control-Allow-Origin`.
    // A properly configured Mastodon serves `/system/*` with `*` from nginx;
    // we do it here so the header travels with the response.
    let media_routes = Router::new()
        .route("/media/{file_name}", get(media::serve))
        // Proxy every kind of remote media through the instance (no origin URL
        // ever reaches a client). Public, like `/media/{file}` itself.
        .route("/media/proxy/{kind}/{id}", get(media::proxy))
        .route("/media/proxy/{kind}/{id}/small", get(media::proxy_small))
        // Caching HLS reverse-proxy (PeerTube long-form video): the player's
        // entry playlist, nested media playlists, and per-range segments — all
        // same-origin so nothing leaks the origin URL to the client.
        .route("/media/hls/{media_id}/master.m3u8", get(hls::master))
        .route("/media/hls/{media_id}/pl", get(hls::playlist))
        .route("/media/hls/{media_id}/seg", get(hls::segment))
        // Live children keep the origin's extension on the path, so players
        // and demuxers that infer a container from the URL get it right.
        .route("/media/hls/{media_id}/live/{name}", get(hls::live_child))
        // The universal live lane: one endless fragmented MP4 for every client
        // that only understands a plain video URL (no-JS, ExoPlayer, VLC).
        .route(
            "/media/live/{media_id}/stream.mp4",
            get(live_stream::stream),
        )
        // Stable single-file compatibility URL for clients that do not know
        // Plamenu's additive HLS extension.
        .route(
            "/media/play/{media_id}/video.mp4",
            get(progressive::get).head(progressive::head),
        )
        // Plain remote audio starts from the first bounded origin range while
        // the same bytes are written into the shared media cache.
        .route(
            "/media/play/{media_id}/audio",
            get(progressive::audio_get).head(progressive::audio_head),
        )
        .layer(cors_any_origin().allow_methods([Method::GET, Method::HEAD]));

    Router::new()
        .merge(discovery_routes)
        .merge(actor_route)
        .merge(federation_object_routes)
        .merge(api_routes(state.clone()))
        .merge(oauth_token_routes)
        .merge(crate::web::router())
        .merge(media_routes)
        // FEP-ae97 portable actors are deliberately outside the authorized-
        // fetch gate: their client owns inbox polling authorization, while
        // public actor/object dereferencing bootstraps remote delivery.
        .route(
            "/.well-known/apgateway",
            get(gateway::metadata).post(gateway::register),
        )
        .route("/.well-known/apgateway-media", post(gateway::upload_media))
        .route(
            "/.well-known/apgateway-media/{*hashlink}",
            get(gateway::get_media).delete(gateway::delete_media),
        )
        .route(
            "/.well-known/apgateway/{*path}",
            get(gateway::get).post(gateway::post),
        )
        // Liveness stays a constant OK; readiness reflects real datastore
        // reachability so a healthy-looking process with a dead database no
        // longer passes the container health check.
        .route("/health", get(liveness))
        .route("/ready", get(readiness))
        // The whole telemetry surface, including route registration, is
        // absent under the default-off configuration.
        .merge(csp_report_routes(state.config.csp_reporting))
        .route("/inbox", post(inbox::shared_inbox))
        .route("/actor/inbox", post(inbox::shared_inbox))
        .route("/actor/outbox", get(crate::instance_actor::get_outbox))
        .route("/webxdc/{id}", get(webxdc::session))
        .route("/webxdc/{id}/bundle.xdc", get(webxdc::bundle))
        .route("/webxdc/{id}/outbox", get(webxdc::outbox))
        .route("/webxdc/{id}/followers", get(webxdc::followers))
        .route("/webxdc/{id}/inbox", post(inbox::shared_inbox))
        .route("/webxdc/caddy-allow", get(webxdc::certificate_allowed))
        .route("/webxdc-runtime/{id}", get(webxdc::runtime_index))
        .route("/webxdc-runtime/{id}/webxdc.js", get(webxdc::bridge))
        .route("/webxdc-runtime/{id}/_bridge.js", get(webxdc::bridge))
        .route("/webxdc-runtime/{id}/{*path}", get(webxdc::runtime_file))
        .route("/users/{username}/inbox", post(inbox::user_inbox))
        .route("/ap/accounts/{username}/inbox", post(inbox::user_inbox))
        .route(
            "/oauth/authorize",
            get(oauth::authorize_form).post(oauth::authorize_submit),
        )
        // The security-key second factor for 3rd-party sign-in — sessionless
        // like `/oauth/authorize`, keyed off the live "oauth" challenge.
        .route(
            "/oauth/authorize/webauthn/options",
            post(oauth::authorize_webauthn_options),
        )
        .route(
            "/oauth/authorize/webauthn",
            post(oauth::authorize_webauthn_finish),
        )
        // Self-destruct wind-down: everything 410s except sign-in and the
        // data export (Mastodon's `check_self_destruct!`). Inside the rate
        // limiter, mirroring Rack::Attack running before controller filters.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::self_destruct::gate,
        ))
        // Outermost on purpose: throttling runs before any routing/auth work,
        // like Mastodon's Rack::Attack. It no-ops for paths that match no
        // bucket.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::rate_limit::gate,
        ))
        // Dedicated Webxdc hosts are package-only virtual hosts. This is the
        // outermost route layer so absolute package paths cannot fall through
        // to Plamenu's own `/assets`, API, or mutation routes.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            webxdc::runtime_origin_gate,
        ))
        .with_state(state)
}
