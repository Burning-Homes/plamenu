//! The discovery-only network boundary. Every wire attempt is admitted by the
//! caller; no caller-owned member signer, bearer token, cookie or HTML fallback.
use super::{
    FEDERATION_JSON_LIMIT, FederationClient, FederationError, PAGE_MAX_REDIRECTS, content_type,
    is_activitypub_content_type, limited_bytes, retry_after_secs,
};
use crate::signature::RequestSigner;
use plamenu_ap::{ACTIVITY_JSON, LD_JSON_AS};
use reqwest::{Url, header::ACCEPT};
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};

type BudgetFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, FederationError>> + Send + 'a>>;

/// A receipt is issued before EACH send, including signature fallback. Lost
/// receipts must remain charged until the accounting window expires.
pub trait DiscoveryBudget: Send + Sync {
    fn reserve<'a>(&'a self, uri: &'a str) -> BudgetFuture<'a, i64>;
    fn complete(&self, receipt: i64, bytes: usize) -> BudgetFuture<'_, ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryFormat {
    ActivityPub,
    Json,
    /// Anonymous search, suppressing Mastodon's optional asynchronous FASP lookup.
    LocalSearchJson,
    /// A bounded server icon, decoded and rasterized by the caller.
    Image,
}

#[derive(Debug, Clone)]
pub struct DiscoveryDocument {
    pub document: Value,
    pub final_url: String,
    pub body_bytes: usize,
    pub image: Option<Vec<u8>>,
}

impl FederationClient {
    /// Manual redirect walking uses the ordinary guarded resolver and proxy
    /// lanes. The budget callback checks mutable local policy before every hop.
    /// AP is signed only with the instance key; public JSON stays anonymous.
    /// Returned JSON is unverified: adapters must establish canonical identity,
    /// visibility and discovery signals before committing catalogue data.
    pub async fn fetch_discovery(
        &self,
        uri: &str,
        format: DiscoveryFormat,
        budget: &dyn DiscoveryBudget,
    ) -> Result<DiscoveryDocument, FederationError> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.discovery_walk(uri, format, budget),
        )
        .await
        .map_err(|_| FederationError::Stalled("discovery deadline".into()))?
    }

    async fn discovery_walk(
        &self,
        uri: &str,
        format: DiscoveryFormat,
        budget: &dyn DiscoveryBudget,
    ) -> Result<DiscoveryDocument, FederationError> {
        let mut current = self.discovery_url(uri)?;
        let accept = match format {
            DiscoveryFormat::Image => {
                "image/png, image/jpeg, image/webp, image/gif, image/x-icon".into()
            }
            DiscoveryFormat::ActivityPub => format!("{ACTIVITY_JSON}, {LD_JSON_AS}"),
            DiscoveryFormat::Json | DiscoveryFormat::LocalSearchJson => "application/json".into(),
        };
        if format == DiscoveryFormat::ActivityPub && self.fetch_signer.is_none() {
            return Err(FederationError::Admission(
                "discovery needs the instance signer".into(),
            ));
        }
        for hop in 0..=PAGE_MAX_REDIRECTS {
            let (response, ticket) = self
                .discovery_send(&current, &accept, format, budget)
                .await?;
            let status = response.status();
            if status.is_redirection() {
                // Drop without consuming the body. A 304 is not useful for an
                // unconditional discovery fetch and has no special exemption.
                let next = self.redirect_target(&current, &response);
                drop(response);
                budget.complete(ticket, 0).await?;
                if hop == PAGE_MAX_REDIRECTS {
                    break;
                }
                current = self.discovery_url(next?.as_str())?;
                continue;
            }
            if !status.is_success() {
                let retry_after = retry_after_secs(response.headers());
                drop(response);
                budget.complete(ticket, 0).await?;
                return Err(
                    if status.as_u16() == 429 || (status.as_u16() == 503 && retry_after.is_some()) {
                        FederationError::RateLimited {
                            retry_after_secs: retry_after,
                        }
                    } else {
                        FederationError::Status(status.as_u16())
                    },
                );
            }
            let kind = content_type(response.headers());
            let allowed = match format {
                DiscoveryFormat::Image => kind.starts_with("image/"),
                DiscoveryFormat::ActivityPub => is_activitypub_content_type(&kind),
                DiscoveryFormat::Json | DiscoveryFormat::LocalSearchJson => kind
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json")),
            };
            if !allowed {
                drop(response);
                budget.complete(ticket, 0).await?;
                return Err(FederationError::InvalidActor(
                    "unexpected discovery content type".into(),
                ));
            }
            // Failure leaves the full reservation charged, including oversized
            // or truncated decoded streams. serde_json also limits nesting.
            let body = limited_bytes(
                response,
                if format == DiscoveryFormat::Image {
                    262_144
                } else {
                    FEDERATION_JSON_LIMIT
                },
                &current,
            )
            .await?;
            budget.complete(ticket, body.len()).await?;
            let document = if format == DiscoveryFormat::Image {
                Value::Null
            } else {
                serde_json::from_slice(&body)
                    .map_err(|e| FederationError::InvalidActor(e.to_string()))?
            };
            return Ok(DiscoveryDocument {
                document,
                final_url: current.to_string(),
                body_bytes: body.len(),
                image: (format == DiscoveryFormat::Image).then(|| body.clone()),
            });
        }
        Err(FederationError::InvalidUrl(
            "too many discovery redirects".into(),
        ))
    }

    fn discovery_url(&self, uri: &str) -> Result<Url, FederationError> {
        let url = self.checked(uri)?;
        if uri.len() > 2048
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(FederationError::InvalidUrl("invalid discovery URL".into()));
        }
        Ok(url)
    }

    async fn discovery_send(
        &self,
        url: &Url,
        accept: &str,
        format: DiscoveryFormat,
        budget: &dyn DiscoveryBudget,
    ) -> Result<(reqwest::Response, i64), FederationError> {
        let request = match format {
            DiscoveryFormat::ActivityPub => self.signed_get(url, accept, None)?,
            DiscoveryFormat::Json | DiscoveryFormat::Image => {
                self.http_for(url).get(url.clone()).header(ACCEPT, accept)
            }
            DiscoveryFormat::LocalSearchJson => self
                .http_for(url)
                .get(url.clone())
                .header(ACCEPT, accept)
                .header("Mastodon-Async-Refresh-Id", "plamenu-local-search"),
        };
        let ticket = budget.reserve(url.as_str()).await?;
        let response = send_reserved(request, budget, ticket).await?;
        if format == DiscoveryFormat::ActivityPub
            && matches!(response.status().as_u16(), 400 | 401)
            && let Some(headers) = self
                .fetch_signer
                .as_ref()
                .and_then(|signer: &RequestSigner| {
                    signer.sign_get_rfc9421_ed25519(url.as_str(), SystemTime::now())
                })
        {
            drop(response);
            budget.complete(ticket, 0).await?;
            let retry = self
                .http_for(url)
                .get(url.clone())
                .header(ACCEPT, accept)
                .header("Date", headers.date)
                .header("Signature-Input", headers.signature_input)
                .header("Signature", headers.signature);
            let ticket = budget.reserve(url.as_str()).await?;
            return Ok((send_reserved(retry, budget, ticket).await?, ticket));
        }
        Ok((response, ticket))
    }
}

/// A failed send has not supplied a response body. Keep its request charge,
/// but settle the unused body reservation. Interrupted/oversized body reads
/// and crashed workers still retain their conservative full reservation.
async fn send_reserved(
    request: reqwest::RequestBuilder,
    budget: &dyn DiscoveryBudget,
    ticket: i64,
) -> Result<reqwest::Response, FederationError> {
    match request.send().await {
        Ok(response) => Ok(response),
        Err(error) => {
            budget.complete(ticket, 0).await?;
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests;
