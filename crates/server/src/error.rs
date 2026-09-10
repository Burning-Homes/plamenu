//! API error type, rendered as Mastodon-style `{"error": "..."}` JSON.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    /// Mastodon's exact wording for 404s, which some clients match on.
    #[error("Record not found")]
    NotFound,
    /// A suspended account's actor/collections (Mastodon returns `410 Gone`).
    #[error("Gone")]
    Gone,
    #[error("{0}")]
    BadRequest(String),
    #[error("Not acceptable")]
    NotAcceptable,
    #[error("Payload too large")]
    PayloadTooLarge,
    #[error("{0}")]
    PayloadTooLargeWithMessage(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Unprocessable(String),
    #[error("{0}")]
    Conflict(String),
    /// A tripped rate limit, in Mastodon's exact wording (its `Rack::Attack`
    /// `throttled_responder`).
    #[error("Too many requests")]
    TooManyRequests,
    /// An upstream dependency (the translation backend) is unreachable, rate-
    /// limited or over quota — Mastodon answers these `503` with a specific
    /// message.
    #[error("{0}")]
    ServiceUnavailable(String),
    #[error("{0}")]
    BadGateway(String),
    /// A 422 in Mastodon's `ValidationErrorFormatter` shape: the summary
    /// message plus a per-attribute `details` object clients parse
    /// (registration & co.).
    #[error("{message}")]
    Validation {
        message: String,
        details: serde_json::Value,
    },
    #[error("Internal server error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl From<plamenu_db::DbError> for ApiError {
    fn from(err: plamenu_db::DbError) -> Self {
        Self::Internal(Box::new(err))
    }
}

/// Displays an error followed by its full `source()` chain (`a: b: c`).
/// Log with this instead of the bare error wherever an [`ApiError`] can
/// surface: `Internal`'s own Display is just "Internal server error", which
/// buries the root cause the log line exists to record.
pub struct ErrorChain<'a>(pub &'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

impl ApiError {
    /// The error with its source chain attached, for log lines.
    #[must_use]
    pub fn chain(&self) -> ErrorChain<'_> {
        ErrorChain(self)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Gone => StatusCode::GONE,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotAcceptable => StatusCode::NOT_ACCEPTABLE,
            Self::PayloadTooLarge | Self::PayloadTooLargeWithMessage(_) => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::Unauthorized(reason) => {
                tracing::debug!(%reason, "rejecting unauthenticated request");
                StatusCode::UNAUTHORIZED
            }
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Unprocessable(_) | Self::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(source) => {
                tracing::error!(error = %ErrorChain(source.as_ref()), "internal error while handling request");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let body = match &self {
            Self::Validation { message, details } => {
                json!({ "error": message, "details": details })
            }
            _ => json!({ "error": self.to_string() }),
        };
        (status, Json(body)).into_response()
    }
}
