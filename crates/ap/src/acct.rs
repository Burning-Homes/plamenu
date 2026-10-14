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

/// Characters allowed in remote usernames, shared with mention tokenization.
/// Unicode letters, numbers and combining marks preserve the spelling supplied
/// by the remote server; dot/hyphen separators support bridged handles.
#[must_use]
pub fn is_username_char(c: char) -> bool {
    c.is_alphanumeric()
        || unicode_normalization::char::is_combining_mark(c)
        || matches!(c, '_' | '.' | '-')
}

/// Remote account names are independent of local registration constraints.
/// Local account creation applies its ASCII and length limits in the server
/// crate instead of imposing them on federated names.
#[must_use]
pub fn is_valid_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 2048
        && s.chars().all(is_username_char)
        && s.chars().next().is_some_and(|c| {
            !unicode_normalization::char::is_combining_mark(c) && (c.is_alphanumeric() || c == '_')
        })
        && s.chars()
            .last()
            .is_some_and(|c| is_username_char(c) && !matches!(c, '.' | '-'))
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
    fn accepts_unicode_and_long_remote_usernames_without_changing_spelling() {
        for username in [
            "GrüneGera",
            "Gru\u{308}neGera",
            "用户",
            "مستخدم",
            "npub1600yr4qg5vcfp7svf6ysj0008tn7aphnu0gjs6lw5hjn74n0laasjx889v",
        ] {
            let acct = Acct::new(username, "Remote.EXAMPLE").unwrap();
            assert_eq!(acct.username(), username);
            assert_eq!(acct.domain(), "remote.example");
        }
        assert!(Acct::new(&"a".repeat(2048), "remote.example").is_ok());
        assert_eq!(
            Acct::new(&"a".repeat(2049), "remote.example"),
            Err(AcctError::InvalidUsername)
        );
        for username in [
            "\u{308}alice",
            "al\u{200b}ice",
            "alice\n",
            "ali/ce",
            "ali@ce",
        ] {
            assert_eq!(
                Acct::new(username, "remote.example"),
                Err(AcctError::InvalidUsername)
            );
        }
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
