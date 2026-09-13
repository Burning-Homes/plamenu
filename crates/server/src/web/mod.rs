//! The first-party, server-rendered web UI.
//!
//! HTML-first and framework-free: pages are rendered with `maud` (typed,
//! compile-time-checked markup), styled by one embedded stylesheet, and made
//! to work with JavaScript disabled — links navigate, forms POST. JS is
//! layered on later purely as enhancement. Presentation reuses the same
//! `entities` layer the JSON API renders from, so the UI cannot drift from the
//! API.

mod actions;
mod admin;
mod announcements;
mod applications;
mod assets;
mod cleanup;
pub(crate) mod clock;
mod collapse;
mod collections;
mod explore;
mod export;
mod featured_tags;
mod feed;
mod filters;
mod groups;
pub(crate) mod i18n;
mod identity;
mod import;
mod interact;
mod invites;
mod landing;
pub(crate) mod layout;
mod lists;
mod meta;
mod migration;
mod notification_requests;
pub(crate) mod pages;
pub(crate) mod password;
mod people;
mod personal_emojis;
mod reactions;
mod register;
mod relationships;
mod scheduled;
pub mod session;
mod sessions;
mod settings;
mod strikes;
mod thread;
pub(crate) mod two_factor;
mod user_agent;
mod view;
pub(crate) mod webauthn;
pub(crate) mod webxdc;

use axum::Router;
use axum::routing::{get, post};

use crate::AppState;

/// A role badge colour, accepted only as a CSS hex literal.
///
/// The value is operator-supplied and lands inside a `style` attribute on a
/// **public** profile (`--role-color: {colour}`) as well as the admin roles
/// list. `maud` escapes the attribute, so it cannot be broken out of — but an
/// unconstrained value is still a whole CSS declaration list: a `manage_roles`
/// holder could set `red;background-image:url(https://…)` and have every
/// anonymous visitor of a badged profile fetch a third-party URL, or use
/// attribute selectors to exfiltrate other admins' CSRF tokens. A hex literal
/// is the entire legitimate vocabulary here, so anything else is dropped.
///
/// Both writers validate with this, and both renderers filter with it, so a
/// row written before the check existed cannot inject either.
#[must_use]
pub(crate) fn badge_color(color: &str) -> Option<&str> {
    let color = color.trim();
    let hex = color.strip_prefix('#')?;
    (matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(color)
}

/// The web UI routes, mounted at the site root alongside the API and AP
/// surfaces.
///
/// The Mastodon-style `/@handle` and `/@handle/{id}` routes are single-segment
/// captures (`/{handle}`, `/{handle}/{id}`); axum prefers the static routes
/// (`/login`, `/public`, `/web/…`) registered beside them, so they only catch
/// genuine profile/thread URLs. State-changing endpoints live under `/web/` so
/// they can never collide with a handle.
// A flat route table: one `.route(...)` per endpoint reads better in one place
// than split across sub-routers, so the length is by design.
#[allow(clippy::too_many_lines)]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(pages::home))
        .route("/public", get(pages::public))
        .route("/explore", get(explore::posts))
        .route("/explore/hashtags", get(explore::hashtags))
        .route("/explore/links", get(explore::links))
        .route("/people", get(people::people))
        .route("/explore/people", get(people::legacy_redirect))
        .route(
            "/web/suggestions/{id}/dismiss",
            post(actions::dismiss_suggestion),
        )
        .route("/terms-of-service", get(pages::terms_of_service))
        .route("/rules", get(landing::rules_page))
        .route("/staff", get(landing::staff_page))
        .route("/announcements", get(announcements::page))
        .route(
            "/web/announcements/{id}/reactions",
            get(announcements::reactions_fragment),
        )
        .route(
            "/web/announcements/{id}/reaction",
            get(announcements::reaction_picker_page),
        )
        .route(
            "/web/announcements/{id}/react",
            post(announcements::react_selected_action),
        )
        .route(
            "/web/announcements/{id}/react/{name}",
            post(announcements::react_action),
        )
        .route(
            "/web/announcements/{id}/unreact/{name}",
            post(announcements::unreact_action),
        )
        .route(
            "/web/announcements/{id}/dismiss",
            post(announcements::dismiss_action),
        )
        .route("/notifications", get(pages::notifications))
        .route("/notifications/requests", get(notification_requests::page))
        .route(
            "/web/notifications/requests/{id}/accept",
            post(notification_requests::accept_action),
        )
        .route(
            "/web/notifications/requests/{id}/dismiss",
            post(notification_requests::dismiss_action),
        )
        .route("/conversations", get(pages::conversations))
        .route(
            "/web/conversations/{id}/read",
            post(actions::conversation_read),
        )
        .route(
            "/web/conversations/{id}/unread",
            post(actions::conversation_unread),
        )
        .route(
            "/web/conversations/{id}/remove",
            post(actions::conversation_remove),
        )
        .route("/bookmarks", get(pages::bookmarks))
        .route("/favourites", get(pages::favourites))
        .route("/webxdc", get(webxdc::index))
        .route("/webxdc/library", get(webxdc::library))
        .route(
            "/webxdc/library/catalog/{id}",
            get(webxdc::external_catalog),
        )
        .route(
            "/webxdc/library/version/{id}/icon",
            get(webxdc::library_icon),
        )
        .route("/webxdc/new", get(webxdc::new_page))
        .route("/webxdc/open", get(webxdc::open_page))
        .route("/webxdc/session/{id}", get(webxdc::remote_landing))
        .route("/webxdc/session/{id}/play", get(webxdc::play))
        .route("/webxdc/{id}/updates", get(webxdc::updates))
        .route("/webxdc/{id}/realtime", get(webxdc::realtime))
        .route("/web/webxdc", post(webxdc::create))
        .route("/web/webxdc/library", post(webxdc::save_library_app))
        .route(
            "/web/webxdc/library/catalog/{id}/import",
            post(webxdc::import_external_catalog_app),
        )
        .route(
            "/web/webxdc/library/{id}/version",
            post(webxdc::add_library_version),
        )
        .route(
            "/web/webxdc/library/{id}/delete",
            post(webxdc::delete_library_app),
        )
        .route("/web/webxdc/open", post(webxdc::open))
        .route("/web/webxdc/{id}/join", post(webxdc::join))
        .route("/web/webxdc/{id}/leave", post(webxdc::leave))
        .route("/web/webxdc/{id}/guest", post(webxdc::guest_join))
        .route("/web/webxdc/{id}/guest/leave", post(webxdc::guest_leave))
        .route(
            "/web/webxdc/{id}/guest/{guest}/approve",
            post(webxdc::approve_guest),
        )
        .route(
            "/web/webxdc/{id}/guest/{guest}/remove",
            post(webxdc::remove_guest),
        )
        .route(
            "/web/webxdc/{id}/approve/{participant}",
            post(webxdc::approve),
        )
        .route(
            "/web/webxdc/{id}/remove/{participant}",
            post(webxdc::remove),
        )
        .route("/web/webxdc/{id}/close", post(webxdc::close))
        .route("/web/webxdc/{id}/delete", post(webxdc::delete))
        .route("/web/webxdc/{id}/updates", post(webxdc::send_update))
        .route("/search", get(pages::search))
        .route("/web/go", get(pages::go))
        .route("/compose", get(pages::compose_page))
        .route("/web/compose/suggestions", get(pages::compose_suggestions))
        .route("/settings", get(settings::index))
        .route("/settings/profile", get(settings::profile_form))
        .route("/settings/preferences", get(settings::preferences))
        .route("/settings/push", get(settings::push_notifications))
        .route("/settings/languages", get(settings::languages_page))
        .route("/settings/privacy", get(settings::privacy))
        .route("/settings/custom-emojis", get(personal_emojis::index))
        .route("/web/custom-emojis", get(personal_emojis::catalog))
        .route(
            "/settings/custom-emojis/borrow/account/{id}",
            get(personal_emojis::borrow_account),
        )
        .route(
            "/settings/custom-emojis/borrow/status/{id}",
            get(personal_emojis::borrow_status),
        )
        .route(
            "/web/settings/custom-emojis",
            post(personal_emojis::create).layer(axum::extract::DefaultBodyLimit::max(
                crate::media_processing::HARD_MAX_EMOJI_BYTES + 64 * 1024,
            )),
        )
        .route(
            "/web/settings/custom-emojis/{id}/update",
            post(personal_emojis::update),
        )
        .route(
            "/web/settings/custom-emojis/{id}/delete",
            post(personal_emojis::delete),
        )
        .route(
            "/web/settings/custom-emojis/{id}/borrow",
            post(personal_emojis::borrow),
        )
        .route("/settings/filters", get(filters::index))
        .route("/settings/filters/new", get(filters::new_page))
        .route("/settings/filters/{id}", get(filters::edit_form))
        .route("/settings/scheduled", get(scheduled::page))
        .route(
            "/web/settings/scheduled/{id}/reschedule",
            post(scheduled::reschedule_action),
        )
        .route(
            "/web/settings/scheduled/{id}/cancel",
            post(scheduled::cancel_action),
        )
        .route("/settings/account", get(settings::account))
        .route("/settings/statuses-cleanup", get(cleanup::page))
        .route("/web/settings/statuses-cleanup", post(cleanup::save_action))
        .route("/settings/identity-proofs", get(identity::page))
        .route(
            "/web/settings/identity-proofs",
            post(identity::publish).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/web/settings/identity-proofs/delete",
            post(identity::remove).layer(axum::extract::DefaultBodyLimit::max(2048)),
        )
        .route("/settings/aliases", get(migration::aliases_page))
        .route("/settings/migration", get(migration::migration_page))
        .route("/web/settings/aliases", post(migration::add_alias_action))
        .route(
            "/web/settings/aliases/delete",
            post(migration::remove_alias_action),
        )
        .route("/web/settings/migration", post(migration::move_action))
        .route("/settings/export", get(import::page))
        .route("/settings/export/{file}", get(export::download))
        .route("/web/settings/archive", post(export::request_archive))
        .route(
            "/settings/archive/{id}/download",
            get(export::download_archive),
        )
        .route("/settings/import/{id}", get(import::show))
        .route("/settings/import/{id}/failures.csv", get(import::failures))
        .route(
            "/web/settings/import/{id}/confirm",
            post(import::confirm_action),
        )
        .route(
            "/web/settings/import/{id}/delete",
            post(import::delete_action),
        )
        .route("/settings/security", get(two_factor::page))
        // The pre-M33 address of the Security page, kept for old bookmarks.
        .route(
            "/settings/two_factor",
            get(|| async { axum::response::Redirect::permanent("/settings/security") }),
        )
        .route(
            "/web/settings/security/password",
            post(two_factor::password_action),
        )
        .route(
            "/web/settings/two_factor/setup",
            post(two_factor::setup_action),
        )
        .route(
            "/web/settings/two_factor/confirm",
            post(two_factor::confirm_action),
        )
        .route(
            "/web/settings/two_factor/recovery_codes",
            post(two_factor::recovery_codes_action),
        )
        .route(
            "/web/settings/two_factor/disable",
            post(two_factor::disable_action),
        )
        .route(
            "/web/settings/security/sessions/{id}/revoke",
            post(sessions::revoke_session_action),
        )
        .route(
            "/web/settings/security/apps/{id}/revoke",
            post(sessions::revoke_app_action),
        )
        .route(
            "/web/settings/security/sign-in-alert",
            post(two_factor::sign_in_alert_action),
        )
        .route(
            "/web/settings/webauthn/options",
            post(webauthn::register_options),
        )
        .route("/web/settings/webauthn", post(webauthn::register_finish))
        .route(
            "/web/settings/webauthn/{id}/delete",
            post(webauthn::delete_credential),
        )
        .route("/login/webauthn/options", post(webauthn::login_options))
        .route("/login/webauthn", post(webauthn::login_finish))
        .route("/settings/applications", get(applications::index))
        .route("/settings/applications/new", get(applications::new_page))
        .route(
            "/settings/applications/{id}",
            get(applications::manage_page),
        )
        .route(
            "/web/settings/applications",
            post(applications::create_action),
        )
        .route(
            "/web/settings/applications/{id}",
            post(applications::update_action),
        )
        .route(
            "/web/settings/applications/{id}/regenerate",
            post(applications::regenerate_action),
        )
        .route(
            "/web/settings/applications/{id}/delete",
            post(applications::delete_action),
        )
        .route("/settings/relationships", get(relationships::page))
        .route(
            "/web/settings/relationships",
            post(relationships::bulk_action),
        )
        .route("/settings/featured-tags", get(featured_tags::page))
        .route(
            "/web/settings/featured-tags",
            post(featured_tags::add_action),
        )
        .route(
            "/web/settings/featured-tags/{id}/remove",
            post(featured_tags::remove_action),
        )
        .route("/lists", get(lists::index))
        .route("/lists/new", get(lists::new_page))
        .route("/web/lists", post(lists::create_action))
        .route("/groups", get(groups::index))
        .route("/groups/new", get(groups::new_page))
        .route("/web/groups", post(groups::create_action))
        .route("/groups/{id}/manage", get(groups::manage))
        .route("/groups/{id}/members", get(groups::members_page))
        .route("/groups/{id}/requests", get(groups::requests_page))
        .route("/groups/{id}/moderators", get(groups::moderators_page))
        .route("/groups/{id}/reports", get(groups::reports_page))
        .route("/web/groups/{id}/settings", post(groups::settings_action))
        .route("/web/groups/{id}/ban", post(groups::ban_action))
        .route("/web/groups/{id}/unban", post(groups::unban_action))
        .route(
            "/web/groups/{id}/requests/approve",
            post(groups::request_approve_action),
        )
        .route(
            "/web/groups/{id}/requests/reject",
            post(groups::request_reject_action),
        )
        .route(
            "/web/groups/{id}/moderators/add",
            post(groups::moderator_add_action),
        )
        .route(
            "/web/groups/{id}/moderators/remove",
            post(groups::moderator_remove_action),
        )
        .route(
            "/web/groups/{id}/moderators/transfer",
            post(groups::moderator_transfer_action),
        )
        .route(
            "/web/groups/{id}/reports/{report_id}/resolve",
            post(groups::report_resolve_action),
        )
        .route(
            "/web/groups/{id}/posts/{status}/remove",
            post(groups::post_remove_action),
        )
        .route(
            "/web/groups/{id}/posts/{status}/lock",
            post(groups::post_lock_action),
        )
        .route(
            "/web/groups/{id}/posts/{status}/pin",
            post(groups::post_pin_action),
        )
        .route("/lists/{id}", get(lists::timeline))
        .route("/lists/{id}/edit", get(lists::edit_form))
        .route("/lists/{id}/members", get(lists::members_page))
        .route("/web/lists/{id}/edit", post(lists::update_action))
        .route("/web/lists/{id}/delete", post(lists::delete_action))
        .route(
            "/web/lists/{id}/members/add",
            post(lists::add_member_action),
        )
        .route(
            "/web/lists/{id}/members/remove",
            post(lists::remove_member_action),
        )
        .route(
            "/web/accounts/{id}/lists",
            get(lists::account_lists_form).post(lists::account_lists_action),
        )
        .route("/settings/collections", get(collections::index))
        .route("/settings/collections/new", get(collections::new_page))
        .route(
            "/web/settings/collections",
            post(collections::create_action),
        )
        .route("/settings/collections/{id}", get(collections::manage_page))
        .route(
            "/web/settings/collections/{id}",
            post(collections::update_action),
        )
        .route(
            "/web/settings/collections/{id}/delete",
            post(collections::delete_action),
        )
        .route(
            "/web/settings/collections/{id}/members/add",
            post(collections::add_member_action),
        )
        .route(
            "/web/settings/collections/{id}/members/remove",
            post(collections::remove_member_action),
        )
        .route(
            "/web/accounts/{id}/collections",
            get(collections::account_collections_form)
                .post(collections::account_collections_action),
        )
        .route(
            "/web/collections/{id}/leave",
            post(collections::leave_action),
        )
        .route("/settings/strikes", get(strikes::page))
        .route(
            "/web/settings/strikes/{id}/appeal",
            post(strikes::submit_appeal),
        )
        .route("/settings/invites", get(invites::page))
        .route("/settings/invites/new", get(invites::new_page))
        .route("/web/settings/invites", post(invites::create_action))
        .route(
            "/web/settings/invites/{id}/expire",
            post(invites::expire_action),
        )
        .route(
            "/web/settings/profile",
            post(settings::update_profile_action),
        )
        .route(
            "/web/settings/preferences",
            post(settings::update_preferences_action),
        )
        .route(
            "/web/settings/languages",
            post(settings::update_languages_action),
        )
        .route(
            "/web/settings/privacy",
            post(settings::update_privacy_action),
        )
        .route("/web/settings/filters", post(filters::create_action))
        .route("/web/settings/filters/{id}", post(filters::update_action))
        .route(
            "/web/settings/filters/{id}/delete",
            post(filters::delete_action),
        )
        .route(
            "/web/settings/account/email",
            post(settings::update_email_action),
        )
        .route(
            "/web/settings/account/delete",
            post(settings::delete_account_action),
        )
        .route("/tags/{tag}", get(pages::tag))
        .route("/web/tags/{tag}/follow", post(actions::tag_follow))
        .route("/web/tags/{tag}/unfollow", post(actions::tag_unfollow))
        .route("/interact", get(interact::page).post(interact::submit))
        .route(
            "/login",
            get(session::login_form).post(session::login_submit),
        )
        .route("/login/challenge", post(session::login_challenge_submit))
        .route(
            "/signup",
            get(register::signup_form).post(register::signup_submit),
        )
        .route("/auth/confirmation", get(register::confirm))
        .route("/auth/password/new", get(password::request_form))
        .route("/auth/password", post(password::request_submit))
        .route(
            "/auth/password/edit",
            get(password::edit_form).post(password::edit_submit),
        )
        .route("/logout", post(session::logout))
        .route("/web/accounts/switch", post(session::switch_account))
        .route("/web/statuses/{id}", get(pages::status_redirect))
        .route("/web/statuses/{id}/favourite", post(actions::favourite))
        .route("/web/statuses/{id}/unfavourite", post(actions::unfavourite))
        // Group-post vote cluster; upvote == favourite underneath.
        .route("/web/statuses/{id}/upvote", post(actions::upvote))
        .route("/web/statuses/{id}/unupvote", post(actions::unupvote))
        .route("/web/statuses/{id}/downvote", post(actions::downvote))
        .route("/web/statuses/{id}/undownvote", post(actions::undownvote))
        // Event RSVP (E2): a negotiation with the organizer, not a toggle — the
        // verdict arrives later as an Accept/Reject, or never.
        .route("/web/statuses/{id}/participate", post(actions::participate))
        .route(
            "/web/statuses/{id}/unparticipate",
            post(actions::unparticipate),
        )
        .route(
            "/web/statuses/{id}/participants/{account_id}/approve",
            post(actions::approve_participant),
        )
        .route(
            "/web/statuses/{id}/participants/{account_id}/reject",
            post(actions::reject_participant),
        )
        // Cancelling is its own action, not a field in the edit form: it notifies
        // every attendee and is the one thing about an event that must not be
        // doable by accident.
        .route(
            "/web/statuses/{id}/cancel-event",
            post(actions::cancel_event),
        )
        .route("/web/statuses/{id}/reblog", post(actions::reblog))
        .route("/web/statuses/{id}/unreblog", post(actions::unreblog))
        .route("/web/statuses/{id}/bookmark", post(actions::bookmark))
        .route("/web/statuses/{id}/translate", post(actions::translate))
        .route("/web/statuses/{id}/unbookmark", post(actions::unbookmark))
        .route("/web/statuses/{id}/vote", post(actions::vote))
        .route("/web/statuses/{id}/poll", get(pages::poll_fragment))
        .route("/web/statuses/{id}/rsvp", get(pages::rsvp_fragment))
        .route("/web/statuses/{id}/delete", post(actions::delete))
        .route(
            "/web/statuses/{id}/edit",
            get(pages::edit_page).post(actions::edit),
        )
        .route(
            "/web/statuses/{id}/report",
            get(pages::report_page).post(actions::report),
        )
        .route("/web/statuses/{id}/redraft", post(actions::redraft))
        .route("/web/statuses/{id}/pin", post(actions::pin))
        .route("/web/statuses/{id}/unpin", post(actions::unpin))
        .route("/web/statuses/{id}/mute", post(actions::mute))
        .route("/web/statuses/{id}/unmute", post(actions::unmute))
        .route(
            "/web/statuses/{id}/reaction",
            get(pages::reaction_picker_page),
        )
        .route("/web/statuses/{id}/react", post(actions::react_selected))
        .route("/web/statuses/{id}/react/{emoji}", post(actions::react))
        .route("/web/statuses/{id}/unreact/{emoji}", post(actions::unreact))
        .route(
            "/web/statuses/{id}/reactions",
            get(pages::reactions_fragment),
        )
        .route(
            "/web/statuses/{id}/quotes/{quote_id}/revoke",
            post(actions::revoke_quote),
        )
        .route(
            "/web/statuses/{id}/quote_policy",
            post(actions::quote_policy),
        )
        .route("/web/accounts/{id}/follow", post(actions::follow))
        .route(
            "/web/accounts/{id}/remote_history",
            get(actions::remote_history_state).post(actions::remote_history),
        )
        .route("/web/accounts/{id}/unfollow", post(actions::unfollow))
        .route(
            "/web/accounts/{id}/follow_settings",
            post(actions::follow_settings),
        )
        .route("/web/accounts/{id}/note", post(actions::account_note))
        .route("/web/accounts/{id}/endorse", post(actions::endorse))
        .route("/web/accounts/{id}/unendorse", post(actions::unendorse))
        .route("/web/accounts/{id}/mute", post(actions::mute_account))
        .route("/web/accounts/{id}/unmute", post(actions::unmute_account))
        .route("/web/accounts/{id}/block", post(actions::block_account))
        .route("/web/accounts/{id}/unblock", post(actions::unblock_account))
        .route(
            "/web/accounts/{id}/report",
            get(pages::report_account_page).post(actions::report_account),
        )
        .route("/web/domains/block", post(actions::block_domain))
        .route("/web/domains/unblock", post(actions::unblock_domain))
        .route("/assets/app.css", get(assets::css))
        .route("/assets/app.js", get(assets::js))
        .route(assets::EMOJI_CATALOG_PATH, get(assets::emoji_catalog))
        .route("/assets/webxdc-host.js", get(assets::webxdc_host_js))
        .route("/assets/hls.min.js", get(assets::hls_js))
        .route("/manifest.webmanifest", get(assets::manifest))
        .route("/sw.js", get(assets::sw_js))
        .route("/offline", get(assets::offline))
        .route("/pwa/{asset}", get(assets::pwa_image))
        .route("/apple-touch-icon.png", get(assets::apple_touch_icon))
        .route("/custom.css", get(assets::custom_css))
        .route("/favicon.ico", get(assets::favicon))
        .route("/static/missing.png", get(assets::missing_image))
        .merge(admin::router())
        .merge(import_upload_route())
        .merge(compose_route())
        .route("/{handle}", get(pages::profile))
        // A static second segment, so matchit prefers it over the profile
        // thread/engagement param routes below.
        .route("/{handle}/collections/{id}", get(collections::public_page))
        .route("/{handle}/{status_id}", get(pages::thread))
        .route("/{handle}/{status_id}/{list}", get(pages::engagement))
}

/// The CSV-import upload endpoint, isolated so its 20 MB body limit applies to
/// this route alone (the rest of the web UI keeps the small default).
fn import_upload_route() -> Router<AppState> {
    Router::new()
        .route("/web/settings/import", post(import::upload))
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::bulk_import::FILE_SIZE_LIMIT + 1024 * 1024,
        ))
}

/// The compose endpoint, isolated so axum's default 2 MB body limit is turned
/// off for it. Unlike the media API (one file per request), the web composer
/// submits every attachment in a single multipart body, and the ceiling depends
/// on the runtime-configurable `max_media_attachments` — which a static router
/// layer can't see. So the handler applies its own body limit per request,
/// derived from the current setting (see `actions::compose`); a modest
/// multi-image post would otherwise overrun the 2 MB default and multer would
/// reject the truncated stream.
fn compose_route() -> Router<AppState> {
    Router::new()
        .route("/web/compose", post(actions::compose))
        .layer(axum::extract::DefaultBodyLimit::disable())
}

#[cfg(test)]
mod tests {
    #[test]
    fn badge_color_accepts_only_hex_literals() {
        // #rgb, #rgba, #rrggbb and #rrggbbaa, surrounding space tolerated.
        for good in ["#fff", "#ff50", "#ff5050", "#ff5050aa", "  #abc  "] {
            assert_eq!(
                super::badge_color(good),
                Some(good.trim()),
                "{good} is a hex colour"
            );
        }
        // A CSS declaration list smuggled through the colour field is what the
        // `style` attribute would otherwise happily accept: an extra
        // declaration that fetches a third-party URL for every viewer of a
        // badged profile, or an attribute selector that exfiltrates a token
        // from the same page.
        for bad in [
            "",
            "#",
            "red",
            "#gggggg",
            "#fffff",
            "red;background-image:url(https://evil.example/beacon)",
            "#fff;background-image:url(https://evil.example/beacon)",
            "var(--x)",
            "#ff5050 ;",
        ] {
            assert_eq!(super::badge_color(bad), None, "{bad} must be refused");
        }
    }
}
