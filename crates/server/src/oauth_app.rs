//! Shared validation for OAuth application registration and update.
//!
//! Two surfaces create/update the same `oauth_apps` model: the public,
//! anonymous `POST /api/v1/apps` endpoint ([`crate::routes::apps`]) and the
//! signed-in `/settings/applications` web forms ([`crate::web::applications`]).
//! Before this module they validated independently — the web forms enforced
//! Mastodon's `ApplicationExtension` rules while the public endpoint stored
//! whatever it was handed (empty/huge names, script-scheme or fragment-bearing
//! redirect URIs, arbitrary website schemes, unknown scope strings). The rules
//! now live here so both paths share one validator and cannot drift.

use std::fmt;

use crate::routes::oauth::OOB_REDIRECT;

/// Mastodon's `ApplicationExtension` limits.
pub const NAME_LIMIT: usize = 60;
pub const REDIRECT_URIS_LIMIT: usize = 2_000;
pub const WEBSITE_LIMIT: usize = 2_000;

/// Every OAuth scope this server recognises: the families Plamenu enforces plus
/// the full granular set a compatible client may request over
/// `POST /api/v1/apps`. Mirrors Mastodon's Doorkeeper scope list (plus the 4.3
/// `profile` scope). A scope outside this set is rejected at registration so it
/// can never reach the authorization/token flow.
pub const SUPPORTED_SCOPES: &[&str] = &[
    // Families.
    "profile",
    "read",
    "write",
    "follow",
    "push",
    "crypto",
    "admin:read",
    "admin:write",
    // `read:*` granular.
    "read:accounts",
    "read:blocks",
    "read:bookmarks",
    "read:collections",
    "read:favourites",
    "read:filters",
    "read:follows",
    "read:lists",
    "read:mutes",
    "read:notifications",
    "read:search",
    "read:statuses",
    // `write:*` granular.
    "write:accounts",
    "write:blocks",
    "write:bookmarks",
    "write:collections",
    "write:conversations",
    "write:favourites",
    "write:filters",
    "write:follows",
    "write:lists",
    "write:media",
    "write:mutes",
    "write:notifications",
    "write:reports",
    "write:statuses",
    // `admin:read:*` granular.
    "admin:read:accounts",
    "admin:read:reports",
    "admin:read:domain_allows",
    "admin:read:domain_blocks",
    "admin:read:email_domain_blocks",
    "admin:read:ip_blocks",
    "admin:read:canonical_email_blocks",
    // `admin:write:*` granular.
    "admin:write:accounts",
    "admin:write:reports",
    "admin:write:domain_allows",
    "admin:write:domain_blocks",
    "admin:write:email_domain_blocks",
    "admin:write:ip_blocks",
    "admin:write:canonical_email_blocks",
];

/// Whether `scope` is a scope this server recognises.
#[must_use]
pub fn is_supported_scope(scope: &str) -> bool {
    SUPPORTED_SCOPES.contains(&scope)
}

/// Why a submitted application was rejected.
///
/// The rejection is typed rather than pre-rendered because two surfaces raise
/// it: `POST /api/v1/apps` answers in English (an API error string, and the
/// wording Mastodon clients already see), while the web form shows the
/// viewer's language. `Display` is the API wording; the web layer maps the
/// same variant onto a catalog message, so neither surface can drift from the
/// rules here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invalid {
    NameBlank,
    NameTooLong,
    RedirectUriMalformed(String),
    RedirectUriFragment(String),
    RedirectUriScheme(String),
    RedirectUrisMissing,
    RedirectUrisTooLong,
    WebsiteTooLong,
    WebsiteNotHttp,
    UnknownScope(String),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameBlank => f.write_str("Application name can't be blank."),
            Self::NameTooLong => {
                write!(
                    f,
                    "Application name is too long ({NAME_LIMIT} characters max)."
                )
            }
            Self::RedirectUriMalformed(uri) => {
                write!(f, "'{uri}' is not a valid redirect URI.")
            }
            Self::RedirectUriFragment(uri) => {
                write!(f, "Redirect URI '{uri}' must not contain a fragment.")
            }
            Self::RedirectUriScheme(uri) => {
                write!(f, "Redirect URI '{uri}' uses a forbidden scheme.")
            }
            Self::RedirectUrisMissing => f.write_str("At least one redirect URI is required."),
            Self::RedirectUrisTooLong => write!(
                f,
                "Redirect URIs are too long ({REDIRECT_URIS_LIMIT} characters max)."
            ),
            Self::WebsiteTooLong => {
                write!(f, "Website is too long ({WEBSITE_LIMIT} characters max).")
            }
            Self::WebsiteNotHttp => f.write_str("Website must be a valid http(s) URL."),
            Self::UnknownScope(scope) => write!(f, "Unknown scope '{scope}'."),
        }
    }
}

/// Validates a client/application name: required (after trimming) and no longer
/// than [`NAME_LIMIT`] characters.
pub fn validate_name(name: &str) -> Result<(), Invalid> {
    if name.is_empty() {
        return Err(Invalid::NameBlank);
    }
    if name.chars().count() > NAME_LIMIT {
        return Err(Invalid::NameTooLong);
    }
    Ok(())
}

/// Validates one redirect URI: the out-of-band sentinel, or an absolute,
/// fragment-free URL whose scheme is not one that would execute in a browser.
pub fn validate_redirect_uri(uri: &str) -> Result<(), Invalid> {
    if uri == OOB_REDIRECT {
        return Ok(());
    }
    let parsed = url::Url::parse(uri).map_err(|_| Invalid::RedirectUriMalformed(uri.to_owned()))?;
    if parsed.fragment().is_some() {
        return Err(Invalid::RedirectUriFragment(uri.to_owned()));
    }
    if matches!(parsed.scheme(), "javascript" | "data" | "vbscript") {
        return Err(Invalid::RedirectUriScheme(uri.to_owned()));
    }
    Ok(())
}

/// Validates the redirect-URI set: at least one entry, the joined text within
/// [`REDIRECT_URIS_LIMIT`], and every entry individually valid.
pub fn validate_redirect_uris(uris: &[String]) -> Result<(), Invalid> {
    if uris.is_empty() {
        return Err(Invalid::RedirectUrisMissing);
    }
    // `n` URIs joined by `n - 1` newlines, matching how the classic
    // newline-separated form field is stored and length-checked.
    let joined_len =
        uris.iter().map(|u| u.chars().count()).sum::<usize>() + uris.len().saturating_sub(1);
    if joined_len > REDIRECT_URIS_LIMIT {
        return Err(Invalid::RedirectUrisTooLong);
    }
    for uri in uris {
        validate_redirect_uri(uri)?;
    }
    Ok(())
}

/// Validates an optional website value. Returns the trimmed value (or `None`
/// when blank); rejects an over-length value or a non-http(s) URL.
pub fn validate_website(raw: &str) -> Result<Option<String>, Invalid> {
    let website = raw.trim();
    if website.is_empty() {
        return Ok(None);
    }
    if website.chars().count() > WEBSITE_LIMIT {
        return Err(Invalid::WebsiteTooLong);
    }
    let valid =
        url::Url::parse(website).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"));
    if !valid {
        return Err(Invalid::WebsiteNotHttp);
    }
    Ok(Some(website.to_owned()))
}

/// Validates and normalizes a space-separated scope string from the public
/// registration endpoint: every token must be a [supported scope](is_supported_scope),
/// duplicates collapse (first occurrence wins), and an empty request falls back
/// to Doorkeeper's default `read` — matching Mastodon's `POST /api/v1/apps`.
pub fn normalize_scopes(raw: &str) -> Result<String, Invalid> {
    let mut scopes: Vec<&str> = Vec::new();
    for token in raw.split_whitespace() {
        if !is_supported_scope(token) {
            return Err(Invalid::UnknownScope(token.to_owned()));
        }
        if !scopes.contains(&token) {
            scopes.push(token);
        }
    }
    if scopes.is_empty() {
        Ok("read".to_owned())
    } else {
        Ok(scopes.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_required_and_bounded() {
        assert!(validate_name("").is_err());
        assert!(validate_name("Fedi Client").is_ok());
        assert!(validate_name(&"x".repeat(NAME_LIMIT)).is_ok());
        assert!(validate_name(&"x".repeat(NAME_LIMIT + 1)).is_err());
    }

    #[test]
    fn redirect_uri_rules() {
        // The out-of-band sentinel and ordinary https callbacks are accepted.
        assert!(validate_redirect_uri(OOB_REDIRECT).is_ok());
        assert!(validate_redirect_uri("https://app.example/callback").is_ok());
        // Custom app schemes are allowed (native clients use them).
        assert!(validate_redirect_uri("myapp://oauth").is_ok());
        // Script/data schemes and fragments are rejected.
        assert!(validate_redirect_uri("javascript:alert(1)").is_err());
        assert!(validate_redirect_uri("data:text/html,<script>").is_err());
        assert!(validate_redirect_uri("https://app.example/cb#frag").is_err());
        // A relative value is not an absolute URI.
        assert!(validate_redirect_uri("/callback").is_err());
    }

    #[test]
    fn redirect_uri_set_rules() {
        assert!(validate_redirect_uris(&[]).is_err());
        assert!(
            validate_redirect_uris(&["https://a.example/cb".to_owned(), OOB_REDIRECT.to_owned()])
                .is_ok()
        );
        // One bad entry fails the whole set.
        assert!(
            validate_redirect_uris(&[
                "https://a.example/cb".to_owned(),
                "javascript:alert(1)".to_owned(),
            ])
            .is_err()
        );
        // Aggregate length is bounded.
        let long = format!("https://a.example/{}", "p".repeat(REDIRECT_URIS_LIMIT));
        assert!(validate_redirect_uris(&[long]).is_err());
    }

    #[test]
    fn api_wording_is_unchanged_by_the_typed_error() {
        assert_eq!(
            Invalid::NameBlank.to_string(),
            "Application name can't be blank."
        );
        assert_eq!(
            Invalid::RedirectUriScheme("javascript:alert(1)".to_owned()).to_string(),
            "Redirect URI 'javascript:alert(1)' uses a forbidden scheme."
        );
        assert_eq!(
            Invalid::UnknownScope("superuser".to_owned()).to_string(),
            "Unknown scope 'superuser'."
        );
    }

    #[test]
    fn website_rules() {
        assert_eq!(validate_website("   "), Ok(None));
        assert_eq!(
            validate_website(" https://site.example "),
            Ok(Some("https://site.example".to_owned()))
        );
        assert!(validate_website("ftp://site.example").is_err());
        assert!(validate_website("javascript:alert(1)").is_err());
        assert!(
            validate_website(&format!("https://x.example/{}", "p".repeat(WEBSITE_LIMIT))).is_err()
        );
    }

    #[test]
    fn scopes_validate_dedup_and_default() {
        // Empty → Doorkeeper default.
        assert_eq!(normalize_scopes("   "), Ok("read".to_owned()));
        // Families and granular scopes are accepted; whitespace-collapsed.
        assert_eq!(
            normalize_scopes("read write:statuses"),
            Ok("read write:statuses".to_owned())
        );
        // First occurrence wins; duplicates collapse.
        assert_eq!(
            normalize_scopes("read write read"),
            Ok("read write".to_owned())
        );
        // An unknown scope is rejected.
        assert!(normalize_scopes("read superuser").is_err());
        assert!(normalize_scopes("write:everything").is_err());
    }
}
