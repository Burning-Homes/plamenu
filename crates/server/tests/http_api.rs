//! HTTP contract tests share one default binary; individual targets remain selectable.

#[path = "api.rs"]
mod api;
#[path = "client_compat.rs"]
mod client_compat;
#[path = "content_negotiation.rs"]
mod content_negotiation;
#[path = "cors.rs"]
mod cors;
#[path = "forwarded.rs"]
mod forwarded;
#[path = "instance_meta.rs"]
mod instance_meta;
#[path = "rate_limit.rs"]
mod rate_limit;
#[path = "security_headers.rs"]
mod security_headers;
#[path = "wire_shapes.rs"]
mod wire_shapes;
