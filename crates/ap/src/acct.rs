//! `acct:` resource handling (RFC 7565), as used by webfinger lookups.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use thiserror::Error;

const USERPART_ENCODE: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

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
/// The domain is normalized to lowercase IDNA ASCII on parse; the
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
        let domain = normalize_domain(domain)?;
        Ok(Self {
            username: username.to_owned(),
            domain,
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

    /// RFC 7565 URI spelling. The URI's encoded userpart is distinct from
    /// encoding the whole URI as a `WebFinger` query parameter.
    #[must_use]
    pub fn to_uri(&self) -> String {
        format!(
            "acct:{}@{}",
            utf8_percent_encode(&self.username, USERPART_ENCODE),
            self.domain
        )
    }
}

impl FromStr for Acct {
    type Err = AcctError;

    /// Parses `acct:user@domain`, `user@domain` or `@user@domain`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = if s
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("acct:"))
        {
            &s[5..]
        } else {
            s
        };
        let s = s.strip_prefix('@').unwrap_or(s);
        if s.is_empty() {
            return Err(AcctError::Empty);
        }
        let (username, domain) = s.split_once('@').ok_or(AcctError::MissingDomain)?;
        // A WebFinger subject is an acct URI, so Unicode userparts may be
        // percent-encoded even when the requested handle was plain Unicode.
        // Decode once, then apply the same validation as literal usernames;
        // escaped separators, controls and malformed UTF-8 remain forbidden.
        if username.len() > 2048 * 3 {
            return Err(AcctError::InvalidUsername);
        }
        let username = percent_decode_str(username)
            .decode_utf8()
            .map_err(|_| AcctError::InvalidUsername)?;
        Self::new(&username, domain)
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

fn normalize_domain(s: &str) -> Result<String, AcctError> {
    // Ports belong to the authority, not an IDNA label. Do not let URL parsing
    // silently consume userinfo, paths, queries, fragments or escaped delimiters.
    let (host, port) = match s.split_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            port.parse::<u16>().map_err(|_| AcctError::InvalidDomain)?;
            (host, Some(port))
        }
        Some(_) => return Err(AcctError::InvalidDomain),
        None => (s, None),
    };
    let host = idna::domain_to_ascii_cow(host.as_bytes(), idna::AsciiDenyList::STD3)
        .map_err(|_| AcctError::InvalidDomain)?;
    let valid = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !valid {
        return Err(AcctError::InvalidDomain);
    }
    Ok(port.map_or_else(|| host.to_string(), |port| format!("{host}:{port}")))
}

/// Human-readable spelling only. Canonical handles, URLs and policy keys keep
/// their ASCII domain, so displaying an IDN never changes an actor's identity.
#[must_use]
pub fn display_domain(domain: &str) -> Cow<'_, str> {
    if !domain.contains("xn--") {
        return Cow::Borrowed(domain);
    }
    let (host, port) = domain
        .split_once(':')
        .map_or((domain, None), |(host, port)| (host, Some(port)));
    let (unicode, result) = idna::domain_to_unicode(host);
    if result.is_err() {
        return Cow::Borrowed(domain);
    }
    Cow::Owned(match port {
        Some(port) => format!("{unicode}:{port}"),
        None => unicode,
    })
}

/// Displays a remote handle in Unicode without altering the username.
#[must_use]
pub fn display_acct(acct: &str) -> Cow<'_, str> {
    let Some((username, domain)) = acct.split_once('@') else {
        return Cow::Borrowed(acct);
    };
    match display_domain(domain) {
        Cow::Borrowed(_) => Cow::Borrowed(acct),
        Cow::Owned(domain) => Cow::Owned(format!("{username}@{domain}")),
    }
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
    fn encoded_acct_uris_and_unicode_handles_have_one_identity() {
        for (username, domain, uri) in [
            (
                "你好",
                "i18n.example",
                "acct:%E4%BD%A0%E5%A5%BD@i18n.example",
            ),
            (
                "GrüneGera",
                "loops.example",
                "acct:Gr%C3%BCneGera@loops.example",
            ),
            (
                "groeibschi",
                "peertube.gröibschi.example",
                "acct:groeibschi@peertube.xn--gribschi-o4a.example",
            ),
            (
                "Gru\u{308}neGera",
                "unicode.example",
                "acct:Gru%CC%88neGera@unicode.example",
            ),
        ] {
            let literal = Acct::new(username, domain).unwrap();
            assert_eq!(literal.to_uri(), uri);
            assert_eq!(uri.parse::<Acct>().unwrap(), literal);
            assert_eq!(
                uri.to_lowercase().parse::<Acct>().unwrap().domain(),
                literal.domain()
            );
            assert_eq!(literal.to_string().parse::<Acct>().unwrap(), literal);
        }
        assert_eq!(
            "acct:%61lice@example.com"
                .parse::<Acct>()
                .unwrap()
                .username(),
            "alice"
        );
        assert_eq!(
            "ACCT:%e4%bd%a0%e5%a5%bd@i18n.example"
                .parse::<Acct>()
                .unwrap(),
            Acct::new("你好", "i18n.example").unwrap()
        );
        for userpart in [
            "%",
            "%0",
            "%GG",
            "%FF",
            "%C0%AF",
            "%E4%BD",
            "%2500",
            "%252F",
            "a%40other",
            "a%2Fb",
            "a%3Ab",
            "a%3Fb",
            "a%23b",
            "a%00b",
            "a%20b",
            "a%0Ab",
            "%CC%88alice",
        ] {
            assert_eq!(
                format!("acct:{userpart}@example.com").parse::<Acct>(),
                Err(AcctError::InvalidUsername),
                "{userpart}"
            );
        }
        assert!(
            format!("acct:{}@example.com", "%61".repeat(2048))
                .parse::<Acct>()
                .is_ok()
        );
        assert!(
            format!("acct:{}@example.com", "%61".repeat(2049))
                .parse::<Acct>()
                .is_err()
        );
        assert!("acct:alice@%65xample.com".parse::<Acct>().is_err());
    }

    #[test]
    fn accepts_domain_with_port() {
        let acct: Acct = "alice@plamenu.local:8420".parse().unwrap();
        assert_eq!(acct.domain(), "plamenu.local:8420");
    }

    #[test]
    fn unicode_and_ascii_domains_have_one_canonical_identity() {
        let ascii = "xn--e1aghfa.xn--e1aqbccjfc.xn--p1acf";
        let unicode = "лемми.опенсорс.рус";
        let canonical = Acct::new("Аим_Нискажуя", ascii).unwrap();
        for domain in [
            unicode,
            "ЛЕММИ.ОПЕНСОРС.РУС",
            ascii,
            "лемми.опенсорс.xn--p1acf",
        ] {
            let acct = Acct::new("Аим_Нискажуя", domain).unwrap();
            assert_eq!(acct, canonical);
            assert_eq!(
                display_acct(&acct.to_string()),
                format!("Аим_Нискажуя@{unicode}")
            );
        }
        assert_eq!(
            Acct::new("alice", "bücher.example").unwrap().domain(),
            "xn--bcher-kva.example"
        );
        assert_eq!(
            Acct::new("alice", "bu\u{308}cher.example")
                .unwrap()
                .domain(),
            "xn--bcher-kva.example"
        );
        assert_eq!(
            Acct::new("alice", "bücher。example").unwrap().domain(),
            "xn--bcher-kva.example"
        );
        assert_eq!(
            Acct::new("alice", "bücher.example:8420").unwrap().domain(),
            "xn--bcher-kva.example:8420"
        );
        assert_eq!(
            display_domain("xn--bcher-kva.example:8420"),
            "bücher.example:8420"
        );
        assert_eq!(display_acct("local_user"), "local_user");
        assert_eq!(display_acct("alice@remote.example"), "alice@remote.example");
        assert_eq!(display_domain("xn--.example"), "xn--.example");
    }

    #[test]
    fn idna_validation_rejects_invalid_domains_and_measures_ascii_dns_lengths() {
        for domain in [
            "",
            ".example",
            "example.",
            "ex..ample",
            "bad_name.example",
            "example.com/path",
            "example.com?query",
            "example.com#fragment",
            "example.com@other.example",
            "%65xample.com",
            "xn--.example",
            "example.com:80:90",
            "example.com:",
            "example.com:65536",
            "ex ample.com",
            "example.com\n",
            "\u{200d}.example",
        ] {
            assert_eq!(
                Acct::new("alice", domain),
                Err(AcctError::InvalidDomain),
                "{domain:?}"
            );
        }
        assert!(Acct::new("alice", &format!("{}.example", "ü".repeat(58))).is_err());
        assert!(Acct::new("alice", &format!("{}.example", "a".repeat(64))).is_err());
        assert!(
            Acct::new(
                "alice",
                &format!(
                    "{}.{}.{}.{}",
                    "a".repeat(63),
                    "b".repeat(63),
                    "c".repeat(63),
                    "d".repeat(62)
                )
            )
            .is_err()
        );
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
