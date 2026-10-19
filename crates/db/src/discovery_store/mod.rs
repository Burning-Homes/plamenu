//! Bounded discovery persistence. Mutations take the settings row lock first,
//! then source/job/entry locks, in a short transaction with no network work.
//! Counts include retained withdrawal guards. Exhaustion rejects new admission
//! instead of evicting a guard or growing an auxiliary hints/history table.

mod browse;
mod presentation;
pub use presentation::*;
mod budget;
mod catalogue;
mod directory;
mod expansion;
mod posts;
mod queue;
mod recovery;
mod sampling;
pub use recovery::*;
mod settings;
mod sources;
mod topics;
mod videos;
mod wider;

pub use browse::*;
pub use budget::*;
pub use catalogue::*;
pub use directory::*;
pub use expansion::*;
pub use posts::*;
pub use queue::*;
pub use sampling::*;
pub use settings::*;
pub use sources::*;
pub use topics::*;
pub use videos::*;
pub use wider::*;

use crate::{DbError, PgPool};
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;

pub(crate) type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Added(i64),
    Existing(i64),
    Disabled,
    Capacity,
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Committed,
    Stale,
    Capacity,
}

fn invalid(message: &str) -> DbError {
    DbError::Protocol(message.to_owned())
}

fn bounded(value: &str, max: usize) -> Result<(), DbError> {
    if value.len() > max || value.contains('\0') {
        Err(invalid("discovery field exceeds its bound or contains NUL"))
    } else {
        Ok(())
    }
}

/// Normalize authority and derive quota identity here so callers cannot label
/// the same origin differently. Network/SSRF admission belongs to transport.
fn address(value: &str) -> Result<(String, String), DbError> {
    bounded(value, 2048)?;
    let url = url::Url::parse(value).map_err(|_| invalid("invalid discovery URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "discovery URL needs an HTTP origin without credentials or fragment",
        ));
    }
    let origin = url.origin().ascii_serialization();
    bounded(&origin, 512)?;
    bounded(url.as_str(), 2048)?;
    Ok((url.to_string(), origin))
}

fn evidence_times(checked: OffsetDateTime, expiry: OffsetDateTime) -> Result<(), DbError> {
    let now = OffsetDateTime::now_utc();
    if checked > now || expiry <= now || expiry <= checked {
        Err(invalid(
            "discovery evidence must be current and have a valid interval",
        ))
    } else {
        Ok(())
    }
}

async fn introducer_valid(tx: &mut Tx<'_>, source: Option<i64>) -> Result<bool, DbError> {
    if let Some(id) = source {
        Ok(
            sqlx::query_scalar!("SELECT enabled FROM discovery_sources WHERE id=$1", id)
                .fetch_optional(&mut **tx)
                .await?
                .unwrap_or(false),
        )
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
