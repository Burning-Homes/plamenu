//! Hidden-service awareness for outbound routing: which overlay network a
//! destination host belongs to, and which URL scheme it conventionally
//! speaks. Ported from Mitra (`apx_core/src/url/hostname.rs`,
//! `apx_sdk/src/http_client.rs`) — the function names mirror Mitra's so the
//! two implementations stay easy to diff.

/// The overlay network a destination host is reached through. Selects the
/// outbound proxy lane; `Default` is the clearnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Default,
    Tor,
    I2p,
}

/// Case-insensitive host-suffix check: DNS-style names compare
/// case-insensitively, and hosts arriving from acct strings (unlike
/// `Url::host_str`) are not pre-lowercased.
fn host_has_suffix(host: &str, suffix: &str) -> bool {
    host.len() >= suffix.len()
        && host.as_bytes()[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

#[must_use]
pub fn is_onion(host: &str) -> bool {
    host_has_suffix(host, ".onion")
}

#[must_use]
pub fn is_i2p(host: &str) -> bool {
    host_has_suffix(host, ".i2p")
}

/// A host that only exists inside an overlay network: it never resolves via
/// DNS and is only reachable through the network's proxy.
#[must_use]
pub fn is_hidden_service(host: &str) -> bool {
    is_onion(host) || is_i2p(host)
}

#[must_use]
pub fn network_type(host: &str) -> Network {
    if is_onion(host) {
        Network::Tor
    } else if is_i2p(host) {
        Network::I2p
    } else {
        Network::Default
    }
}

/// The URL scheme to construct for a bare hostname (`WebFinger`, host-meta):
/// hidden services conventionally serve plain HTTP — hardcoding `https`
/// there is the Pleroma bug that breaks handle discovery of onion accounts.
#[must_use]
pub fn guess_protocol(host: &str) -> &'static str {
    if is_hidden_service(host) {
        "http"
    } else {
        "https"
    }
}

/// Whether a remote-supplied URL is acceptable as a federation reference
/// (an actor/object id, media URL, key id): `https://` everywhere, and
/// `http://` only on hidden-service hosts — the same rule as
/// [`crate::guard::check_url_syntax`], for the ingest-side filters that used
/// to spell it `starts_with("https://")`.
#[must_use]
pub fn is_federation_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => parsed.host_str().is_some(),
        "http" => parsed.host_str().is_some_and(is_hidden_service),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_service_suffixes_are_detected() {
        assert!(is_onion(
            "ajzihxukudausqdndsz4qmromzlo6yt5r2kqa77btk7ghxxzsbb3a4id.onion"
        ));
        assert!(is_i2p("example.i2p"));
        assert!(is_hidden_service("example.onion"));
        assert!(is_hidden_service("example.i2p"));

        assert!(!is_hidden_service("example.com"));
        assert!(!is_hidden_service("onion.example.com"));
        // A suffix check, not a substring check.
        assert!(!is_hidden_service("example.onion.com"));
        assert!(!is_hidden_service("my-onion"));
        // DNS-style names compare case-insensitively.
        assert!(is_onion("EXAMPLE.ONION"));
        assert!(is_i2p("Example.I2P"));
    }

    #[test]
    fn network_type_maps_to_lanes() {
        assert_eq!(network_type("example.onion"), Network::Tor);
        assert_eq!(network_type("example.i2p"), Network::I2p);
        assert_eq!(network_type("example.com"), Network::Default);
    }

    #[test]
    fn guess_protocol_matches_mitra() {
        assert_eq!(guess_protocol("example.onion"), "http");
        assert_eq!(guess_protocol("example.i2p"), "http");
        assert_eq!(guess_protocol("example.com"), "https");
    }

    #[test]
    fn federation_urls_allow_http_only_for_hidden_hosts() {
        assert!(is_federation_url("https://example.com/users/a"));
        assert!(is_federation_url("http://xyz.onion/users/a#main-key"));
        assert!(is_federation_url("http://xyz.i2p/media/1.png"));

        assert!(!is_federation_url("http://example.com/users/a"));
        assert!(!is_federation_url("http://example.onion.com/users/a"));
        assert!(!is_federation_url("ftp://example.com/x"));
        assert!(!is_federation_url("not a url"));
        assert!(!is_federation_url("data:text/html,hi"));
    }
}
