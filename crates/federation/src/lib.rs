//! Federation transport: HTTP signatures and the outbound HTTP client for
//! actor dereferencing and activity delivery.
//!
//! Two signature dialects coexist on the wire: draft-cavage (the fediverse
//! de facto standard — everything speaks it) and RFC 9421 (verified by
//! Mastodon ≥4.4 and Mitra). Outbound deliveries can double-knock: try
//! RFC 9421 first, fall back to draft-cavage on rejection, with the caller
//! remembering each host's answer.

pub mod client;
pub mod digest;
pub mod guard;
pub mod network;
pub mod request_auth;
pub mod rfc9421;
pub mod signature;

pub use client::{
    FederationClient, FederationError, FetchedActivityPub, FetchedMedia, FetchedMediaFile,
    FetchedMediaRange, FetchedPage, HttpMethod, ProxyConfig, ResolvedAcct, ServiceResponse,
    SignatureStyle, WebfingerCandidate,
};
pub use network::{
    Network, guess_protocol, is_federation_url, is_hidden_service, is_i2p, is_onion, network_type,
};
pub use request_auth::PreparedRequestAuth;
pub use rfc9421::{PreparedRfc9421, RequestFacts};
pub use signature::{PreparedVerification, RequestSigner, SignatureError};
