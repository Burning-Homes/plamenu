//! `acct:` resource handling (RFC 7565), as used by webfinger lookups.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AcctError {
    #[error("resource is empty")]
    Empty,
    #[error("missing domain part")]
    MissingDomain,
    #[error("invalid username")]
    InvalidUsername,
    #[error("invalid domain")]
    InvalidDomain,
}

/// A parsed `user@domain` pair.
///
/// The domain is lowercased on parse (host names are case-insensitive); the
/// username keeps its original case and must be compared case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acct {
    username: String,
    domain: String,
}

impl Acct {
    pub fn new(username: &str, domain: &str) -> Result<Self, AcctError> {
        if !is_valid_username(username) {
            return Err(AcctError::InvalidUsername);
        }
        if !is_valid_domain(domain) {
            return Err(AcctError::InvalidDomain);
        }
        Ok(Self {
            username: username.to_owned(),
            domain: domain.to_ascii_lowercase(),
        })
    }

    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    #[must_use]
    pub fn domain(&self) -> &str {
        &self.domain
    }
}

impl FromStr for Acct {
    type Err = AcctError;

    /// Parses `acct:user@domain`, `user@domain` or `@user@domain`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix("acct:").unwrap_or(s);
        let s = s.strip_prefix('@').unwrap_or(s);
        if s.is_empty() {
            return Err(AcctError::Empty);
        }
        let (username, domain) = s.split_once('@').ok_or(AcctError::MissingDomain)?;
        Self::new(username, domain)
    }
}

impl fmt::Display for Acct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.username, self.domain)
    }
}

/// Remote usernames use Mastodon's federated rule: ASCII alphanumerics and
/// underscores, with dot/hyphen separator runs between non-separator segments.
/// Local account creation keeps its stricter `[a-z0-9_]+`, 30-char rule in the
/// server crate.
fn is_valid_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 2048
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && s.bytes()
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_valid_domain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with('.')
        && !s.ends_with('.')
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        // Allow an explicit port for development domains like `plamenu.local:8420`.
        || s.split_once(':').is_some_and(|(host, port)| {
            is_valid_domain(host) && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_forms() {
        for input in [
            "alice@example.com",
            "acct:alice@example.com",
            "@alice@example.com",
        ] {
            let acct: Acct = input.parse().unwrap();
            assert_eq!(acct.username(), "alice");
            assert_eq!(acct.domain(), "example.com");
        }
    }

    #[test]
    fn lowercases_domain_but_not_username() {
        let acct: Acct = "Alice@Example.COM".parse().unwrap();
        assert_eq!(acct.username(), "Alice");
        assert_eq!(acct.domain(), "example.com");
    }

    #[test]
    fn accepts_domain_with_port() {
        let acct: Acct = "alice@plamenu.local:8420".parse().unwrap();
        assert_eq!(acct.domain(), "plamenu.local:8420");
    }

    #[test]
    fn accepts_mastodon_remote_username_rule() {
        let acct: Acct = "bob.bsky.social@bsky.brid.ge".parse().unwrap();
        assert_eq!(acct.username(), "bob.bsky.social");
        assert_eq!(acct.domain(), "bsky.brid.ge");

        let hyphenated: Acct = "blue-sky.bridge_user@example.com".parse().unwrap();
        assert_eq!(hyphenated.username(), "blue-sky.bridge_user");
    }

    #[test]
    fn rejects_malformed_resources() {
        assert_eq!("".parse::<Acct>(), Err(AcctError::Empty));
        assert_eq!("acct:".parse::<Acct>(), Err(AcctError::Empty));
        assert_eq!("alice".parse::<Acct>(), Err(AcctError::MissingDomain));
        assert_eq!(
            "al ice@example.com".parse::<Acct>(),
            Err(AcctError::InvalidUsername)
        );
        assert_eq!(
            ".alice@example.com".parse::<Acct>(),
            Err(AcctError::InvalidUsername)
        );
        assert_eq!(
            "alice.@example.com".parse::<Acct>(),
            Err(AcctError::InvalidUsername)
        );
        assert_eq!("alice@".parse::<Acct>(), Err(AcctError::InvalidDomain));
        assert_eq!(
            "alice@ex ample.com".parse::<Acct>(),
            Err(AcctError::InvalidDomain)
        );
        // `split_once` keeps everything after the first `@` as the domain part.
        assert_eq!("a@b@c".parse::<Acct>(), Err(AcctError::InvalidDomain));
    }

    #[test]
    fn displays_canonical_form() {
        let acct: Acct = "acct:alice@Example.com".parse().unwrap();
        assert_eq!(acct.to_string(), "alice@example.com");
    }
}
