use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::AppState;
use crate::auth::{AdminUser, CurrentUser};

use super::error::LemmyError;

/// Authenticated Lemmy client. The credential is an ordinary Plamenu OAuth
/// bearer token; the wrapper only changes rejection semantics to Lemmy's JSON
/// error contract.
pub struct LemmyUser(pub CurrentUser);

impl FromRequestParts<AppState> for LemmyUser {
    type Rejection = LemmyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        CurrentUser::from_request_parts(parts, state)
            .await
            .map(Self)
            .map_err(LemmyError::from)
    }
}

/// A Plamenu staff token with its role, rendered through Lemmy's error
/// contract. Individual handlers still require the exact permission bit.
pub struct LemmyAdmin(pub AdminUser);

impl FromRequestParts<AppState> for LemmyAdmin {
    type Rejection = LemmyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        AdminUser::from_request_parts(parts, state)
            .await
            .map(Self)
            .map_err(LemmyError::from)
    }
}

/// Optional authentication for public Lemmy endpoints. A malformed or stale
/// bearer is still rejected instead of being silently treated as anonymous.
pub struct MaybeLemmyUser(pub Option<CurrentUser>);

impl FromRequestParts<AppState> for MaybeLemmyUser {
    type Rejection = LemmyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if parts.headers.get(AUTHORIZATION).is_none() {
            return Ok(Self(None));
        }
        LemmyUser::from_request_parts(parts, state)
            .await
            .map(|user| Self(Some(user.0)))
    }
}
