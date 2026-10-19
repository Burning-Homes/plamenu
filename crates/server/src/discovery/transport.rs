//! The worker's only network entry point. All auxiliary fetches and future
//! adapters must share this lease-scoped budget through `FederationApi`.
use crate::{AppState, federation::BoxFuture};
use plamenu_db::discovery_store::{self as db, Lease, Reservation};
use plamenu_federation::{DiscoveryBudget, DiscoveryDocument, DiscoveryFormat, FederationError};

pub struct Budget<'a> {
    pub state: &'a AppState,
    pub lease: &'a Lease,
}

fn db_error(error: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(error.to_string())
}

impl Budget<'_> {
    pub async fn allowed(&self) -> Result<bool, FederationError> {
        if self.state.shutdown.is_cancelled() {
            return Ok(false);
        }
        Ok(db::lease_is_current(&self.state.pool, self.lease)
            .await
            .map_err(|error| db_error(&error))?
            && crate::instance_policy::can_federate_url(
                &self.state.pool,
                &self.state.config.domain,
                &self.lease.target_uri,
            )
            .await
            .map_err(|error| db_error(&error))?)
    }
}

impl Budget<'_> {
    fn reserve_for<'a>(
        &'a self,
        uri: &'a str,
        icon: bool,
    ) -> BoxFuture<'a, Result<i64, FederationError>> {
        Box::pin(async move {
            let same_origin = url::Url::parse(uri)
                .ok()
                .zip(url::Url::parse(&self.lease.target_uri).ok())
                .is_some_and(|(a, b)| a.origin() == b.origin());
            if !same_origin
                && (!icon
                    || !db::request_origin_allowed(&self.state.pool, self.lease, uri)
                        .await
                        .map_err(|e| db_error(&e))?)
            {
                return Err(FederationError::InvalidUrl(
                    "discovery redirect left its source/icon origin".into(),
                ));
            }
            if self.state.shutdown.is_cancelled() {
                return Err(FederationError::DiscoveryStale);
            }
            if !crate::instance_policy::can_federate_url(
                &self.state.pool,
                &self.state.config.domain,
                uri,
            )
            .await
            .map_err(|error| db_error(&error))?
            {
                return Err(FederationError::DiscoveryDeferred("instance_blocked"));
            }
            // Respect the shared host breaker as well as discovery's own
            // endpoint cooldowns. No new failure in discovery spends a user's
            // authorization budget.
            let host = url::Url::parse(uri)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned));
            if let Some(host) = host
                && !plamenu_db::remote_fetch_failure::should_attempt(
                    &self.state.pool,
                    "host",
                    &host,
                )
                .await
                .map_err(|error| db_error(&error))?
            {
                return Err(FederationError::DiscoveryDeferred("host_backoff"));
            }
            match db::reserve_request(&self.state.pool, self.lease, uri)
                .await
                .map_err(|error| db_error(&error))?
            {
                Reservation::Reserved(id) => Ok(id),
                Reservation::Stale => Err(FederationError::DiscoveryStale),
                Reservation::Capacity => Err(FederationError::DiscoveryDeferred("discovery_busy")),
                Reservation::Budget => Err(FederationError::DiscoveryDeferred("budget_exhausted")),
            }
        })
    }
}
impl DiscoveryBudget for Budget<'_> {
    fn reserve<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<i64, FederationError>> {
        self.reserve_for(uri, false)
    }
    fn complete(&self, receipt: i64, bytes: usize) -> BoxFuture<'_, Result<(), FederationError>> {
        Box::pin(async move {
            let bytes = i64::try_from(bytes)
                .map_err(|_| FederationError::Admission("invalid byte count".into()))?;
            db::complete_request(&self.state.pool, receipt, bytes)
                .await
                .map_err(|error| db_error(&error))
        })
    }
}

struct IconBudget<'a>(Budget<'a>);
impl DiscoveryBudget for IconBudget<'_> {
    fn reserve<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<i64, FederationError>> {
        self.0.reserve_for(uri, true)
    }
    fn complete(&self, receipt: i64, bytes: usize) -> BoxFuture<'_, Result<(), FederationError>> {
        self.0.complete(receipt, bytes)
    }
}
pub async fn fetch(
    state: &AppState,
    lease: &Lease,
    uri: &str,
    format: DiscoveryFormat,
) -> Result<DiscoveryDocument, FederationError> {
    if format == DiscoveryFormat::Image {
        state
            .federation
            .fetch_discovery(uri, format, &IconBudget(Budget { state, lease }))
            .await
    } else {
        state
            .federation
            .fetch_discovery(uri, format, &Budget { state, lease })
            .await
    }
}
