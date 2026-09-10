mod admin;
mod auth;
mod comment;
mod community;
mod custom_emoji;
mod discovery;
mod entities;
mod error;
mod ids;
mod inbox;
mod media;
mod moderation;
mod post;
mod relationship;
mod report;
mod site;
mod user;

use axum::Router;
use axum::routing::{get, post, put};

use crate::AppState;

/// The Lemmy API version whose wire contract this adapter targets. Plamenu
/// remains Plamenu in `NodeInfo`; this version describes only `/api/v3`.
pub const LEMMY_COMPAT_VERSION: &str = "0.19.11";

// Keeping the compatibility surface in one table makes missing or duplicated
// Lemmy paths visible during review; splitting it would only hide route count.
#[allow(clippy::too_many_lines)]
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v3/site", get(site::get_site).put(site::edit_site))
        .route("/api/v3/user/login", post(user::login))
        .route("/api/v3/user/register", post(user::register))
        .route("/api/v3/user/logout", post(user::logout))
        .route("/api/v3/user/password_reset", post(user::password_reset))
        .route(
            "/api/v3/user/password_change",
            post(user::password_change_after_reset),
        )
        .route("/api/v3/user/change_password", put(user::change_password))
        .route("/api/v3/user/delete_account", post(user::delete_account))
        .route("/api/v3/user/save_user_settings", put(user::save_settings))
        .route("/api/v3/user/validate_auth", get(user::validate_auth))
        .route("/api/v3/user/unread_count", get(user::unread_count))
        .route("/api/v3/user/replies", get(inbox::replies))
        .route("/api/v3/user/mention", get(inbox::mentions))
        .route("/api/v3/user/mark_all_as_read", post(inbox::mark_all_read))
        .route(
            "/api/v3/comment_reply/mark_as_read",
            post(inbox::mark_reply_read),
        )
        .route(
            "/api/v3/person_mention/mark_as_read",
            post(inbox::mark_mention_read),
        )
        .route("/api/v3/private_message/list", get(inbox::private_messages))
        .route(
            "/api/v3/private_message",
            post(inbox::create_private_message).put(inbox::edit_private_message),
        )
        .route(
            "/api/v3/private_message/delete",
            post(inbox::delete_private_message),
        )
        .route(
            "/api/v3/private_message/mark_as_read",
            post(inbox::mark_private_message_read),
        )
        .route("/api/v3/user", get(discovery::person_details))
        .route("/api/v3/resolve_object", get(discovery::resolve_object))
        .route("/api/v3/search", get(discovery::search))
        .route("/api/v3/user/ban", post(admin::ban_person))
        .route("/api/v3/user/banned", get(admin::banned_persons))
        .route(
            "/api/v3/community",
            get(community::get)
                .post(community::create)
                .put(community::edit),
        )
        .route("/api/v3/community/list", get(community::list))
        .route(
            "/api/v3/community/follow",
            post(relationship::follow_community),
        )
        .route(
            "/api/v3/community/block",
            post(relationship::block_community),
        )
        .route("/api/v3/person/block", post(relationship::block_person))
        .route(
            "/api/v3/comment",
            get(comment::get).post(comment::create).put(comment::edit),
        )
        .route("/api/v3/comment/list", get(comment::list))
        .route("/api/v3/comment/like", post(comment::like))
        .route("/api/v3/comment/save", put(comment::save))
        .route("/api/v3/comment/delete", post(comment::delete))
        .route(
            "/api/v3/post",
            get(post::get).post(post::create).put(post::edit),
        )
        .route("/api/v3/post/list", get(post::list))
        .route("/api/v3/post/like", post(post::like))
        .route("/api/v3/post/save", put(post::save))
        .route("/api/v3/post/mark_as_read", post(post::mark_as_read))
        .route("/api/v3/post/delete", post(post::delete))
        .route("/api/v3/post/report", post(report::create_post))
        .route("/api/v3/post/report/list", get(report::list_posts))
        .route("/api/v3/post/report/resolve", put(report::resolve_post))
        .route("/api/v3/comment/report", post(report::create_comment))
        .route("/api/v3/comment/report/list", get(report::list_comments))
        .route(
            "/api/v3/comment/report/resolve",
            put(report::resolve_comment),
        )
        .route("/api/v3/report/count", get(report::count))
        .route(
            "/api/v3/community/ban_user",
            post(moderation::ban_from_community),
        )
        .route("/api/v3/community/mod", post(moderation::add_mod))
        .route("/api/v3/community/hide", put(admin::hide_community))
        .route("/api/v3/community/remove", post(admin::remove_community))
        .route("/api/v3/post/remove", post(moderation::remove_post))
        .route("/api/v3/post/lock", post(moderation::lock_post))
        .route("/api/v3/post/feature", post(moderation::feature_post))
        .route("/api/v3/admin/add", post(admin::add_admin))
        .route(
            "/api/v3/admin/registration_application/count",
            get(admin::registration_count),
        )
        .route(
            "/api/v3/admin/registration_application/list",
            get(admin::list_registrations),
        )
        .route(
            "/api/v3/admin/registration_application",
            get(admin::get_registration),
        )
        .route(
            "/api/v3/admin/registration_application/approve",
            put(admin::approve_registration),
        )
        .route("/api/v3/admin/purge/person", post(admin::purge_person))
        .route(
            "/api/v3/admin/purge/community",
            post(admin::purge_community),
        )
        .route("/api/v3/admin/purge/post", post(admin::purge_post))
        .route("/api/v3/admin/purge/comment", post(admin::purge_comment))
        .route(
            "/api/v3/federated_instances",
            get(admin::federated_instances),
        )
        .route("/api/v3/site/block", post(relationship::block_instance))
        .route("/pictrs/image", post(media::upload))
        .route(
            "/pictrs/image/delete/{token}/{filename}",
            get(media::delete),
        )
        .route("/pictrs/image/{filename}", get(media::serve))
        .route("/api/v3/account/list_media", get(media::list_owned))
        .route("/api/v3/admin/list_all_media", get(media::list_all))
        .route(
            "/api/v3/custom_emoji",
            post(custom_emoji::create).put(custom_emoji::edit),
        )
        .route("/api/v3/custom_emoji/delete", post(custom_emoji::delete))
        .layer(axum::middleware::from_fn_with_state(
            state,
            ids::translate_response,
        ))
}
