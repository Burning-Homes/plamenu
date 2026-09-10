use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::ApiError;

/// Lemmy's client contract uses a stable symbolic `error` value rather than
/// Mastodon's human-facing message. Keep the two protocols isolated at this
/// boundary even when they share the same underlying service error.
#[derive(Debug)]
pub struct LemmyError {
    pub status: StatusCode,
    pub code: &'static str,
}

impl LemmyError {
    pub const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    pub const fn unauthorized(code: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code)
    }

    pub const fn forbidden(code: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code)
    }

    pub const fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
}

impl IntoResponse for LemmyError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

impl From<ApiError> for LemmyError {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::Unauthorized(_) => Self::unauthorized("not_logged_in"),
            ApiError::Forbidden(_) => Self::forbidden("not_an_admin"),
            ApiError::NotFound => Self::new(StatusCode::NOT_FOUND, "not_found"),
            ApiError::Unprocessable(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_form")
            }
            ApiError::TooManyRequests => {
                Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
            }
            _ => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "unknown"),
        }
    }
}

impl From<plamenu_db::DbError> for LemmyError {
    fn from(error: plamenu_db::DbError) -> Self {
        Self::from(ApiError::from(error))
    }
}
