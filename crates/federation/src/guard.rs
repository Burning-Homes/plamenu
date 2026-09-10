//! Outbound request guard: keeps federation traffic away from internal
//! addresses (SSRF). A remote actor controls many of the URLs we fetch and
//! deliver to, so every outbound request is checked first.
//!
//! The HTTP clients install [`SafeResolver`], so the addresses checked here
//! are the same addresses handed to the connector. This avoids the classic
//! resolve-check-resolve DNS-rebinding race.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::FederationError;

const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// A reqwest resolver that rejects private answers before returning the
/// accepted addresses to the connector. Resolution itself has a hard wall
/// clock limit, so a wedged system resolver cannot hold a federation task
/// forever.
pub struct SafeResolver {
    allow_private: bool,
}

impl SafeResolver {
    #[must_use]
    pub fn shared(allow_private: bool) -> Arc<Self> {
        Arc::new(Self { allow_private })
    }
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let resolved = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((&*host, 0)))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "DNS lookup timed out")
                })??;
            let addrs = vet_answers(&host, resolved.collect(), allow_private)?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Vets one DNS answer set. A private answer alongside public ones is dropped
/// rather than fatal: real hosts publish broken records next to working ones
/// (a link-local `fe80::` AAAA leaked from the server's own interface, seen in
/// the wild beside a fine A record), and refusing the whole host would
/// eventually feed its finite failure budget and blackhole it. SSRF safety
/// comes from the surviving list — the connector never sees what is dropped
/// here — so only an answer set with nothing public left is an error.
fn vet_answers(
    host: &str,
    addrs: Vec<SocketAddr>,
    allow_private: bool,
) -> std::io::Result<Vec<SocketAddr>> {
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{host} resolved to no addresses"),
        ));
    }
    if allow_private {
        return Ok(addrs);
    }
    let public: Vec<_> = addrs
        .into_iter()
        .filter(|addr| !is_private_ip(addr.ip()))
        .collect();
    if public.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{host} resolves only to private addresses"),
        ));
    }
    Ok(public)
}

/// Performs the non-DNS URL checks before handing a request to reqwest. IP
/// literals are checked here because a connector need not invoke its DNS
/// resolver for them.
pub fn check_url_syntax(url: &Url, allow_private: bool) -> Result<(), FederationError> {
    // Hidden services conventionally serve plain HTTP (TLS adds nothing over
    // the overlay network's own encryption); everything else stays
    // https-only — that gate is load-bearing for SSRF.
    let hidden = url
        .host_str()
        .is_some_and(crate::network::is_hidden_service);
    let scheme_allowed = url.scheme() == "https" || (url.scheme() == "http" && hidden);
    if !scheme_allowed {
        return Err(FederationError::InvalidUrl(format!(
            "{url}: only https is allowed"
        )));
    }
    let literal = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    };
    if !allow_private
        && let Some(ip) = literal
        && is_private_ip(ip)
    {
        return Err(FederationError::PrivateAddress(ip.to_string()));
    }
    Ok(())
}

/// `true` for addresses that must never be dialed by federation traffic.
#[must_use]
pub fn is_private_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => is_private_v4(ip),
        IpAddr::V6(ip) => is_private_v6(ip),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        // 0.0.0.0/8 ("this network"); only 0.0.0.0/32 is `is_unspecified`.
        || octets[0] == 0
        // 224.0.0.0/4 (multicast) and 240.0.0.0/4 (reserved/future); neither is
        // a valid unicast federation peer.
        || ip.is_multicast()
        || octets[0] >= 240
        // 100.64.0.0/10 (carrier-grade NAT)
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
        // 192.0.0.0/24 (IETF protocol assignments)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // 192.88.99.0/24 (deprecated 6to4 relay anycast)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        // 198.18.0.0/15 (benchmarking); routed inside some private networks.
        || (octets[0] == 198 && (octets[1] & 0b1111_1110) == 18)
}

/// IPv6 is judged by the *globally-routable-unicast* rule rather than a
/// deny-list of the ranges someone happened to think of.
///
/// A deny-list is the wrong shape here because several non-global IPv6 blocks
/// are not merely unroutable — they *translate to an IPv4 destination* on a
/// host configured for them, so missing one turns into an SSRF bypass rather
/// than a dead connection. `64:ff9b::/96` (NAT64) and `2002::/16` (6to4) both
/// embed an IPv4 address, and `::a.b.c.d` (IPv4-compatible) is an IPv4 address
/// spelled as IPv6; `64:ff9b::10.0.0.1` on a NAT64 network reaches
/// `10.0.0.1`.
///
/// So: everything outside `2000::/3` is refused — that single test covers the
/// loopback, unspecified, IPv4-compatible, NAT64, discard-only (`100::/64`),
/// `SRv6` (`5f00::/16`), unique-local, link-local, site-local and multicast
/// space at once — and the non-global assignments carved out *inside*
/// `2000::/3` are then refused individually. Every IANA global-unicast
/// allocation lives in `2000::/3`, so no real federation peer is excluded.
fn is_private_v6(ip: Ipv6Addr) -> bool {
    // An IPv4-mapped address (`::ffff:a.b.c.d`) is an IPv4 destination wearing
    // an IPv6 spelling: judge it by the IPv4 rules, which still accept public
    // ones.
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_private_v4(mapped);
    }
    let segments = ip.segments();
    // Anything outside global unicast (2000::/3).
    if (segments[0] & 0xe000) != 0x2000 {
        return true;
    }
    // Non-global assignments inside 2000::/3.
    if segments[0] == 0x2001 {
        return
            // 2001::/32 (Teredo)
            segments[1] == 0x0000
            // 2001:2::/48 (benchmarking)
            || (segments[1] == 0x0002 && segments[2] == 0x0000)
            // 2001:10::/28 (deprecated ORCHID)
            || (segments[1] & 0xfff0) == 0x0010
            // 2001:20::/28 (ORCHIDv2)
            || (segments[1] & 0xfff0) == 0x0020
            // 2001:db8::/32 (documentation)
            || segments[1] == 0x0db8;
    }
    // 2002::/16 (6to4): the next 32 bits are an embedded IPv4 address, so a
    // 6to4-capable host dials that address — private ones included.
    if segments[0] == 0x2002 {
        return true;
    }
    // 3fff::/20 (documentation, RFC 9637)
    segments[0] == 0x3fff && (segments[1] & 0xf000) == 0x0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_v4_ranges_are_detected() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.0.5",
            "100.64.0.1",
            "100.127.0.1",
            "0.0.0.0",
            "255.255.255.255",
            // IANA special-purpose ranges beyond the classic RFC1918 set.
            "0.0.0.1",         // 0.0.0.0/8 "this network"
            "192.0.0.1",       // 192.0.0.0/24 protocol assignments
            "192.88.99.1",     // 192.88.99.0/24 deprecated 6to4 anycast
            "198.18.0.1",      // 198.18.0.0/15 benchmarking
            "198.19.255.255",  // upper edge of the benchmarking range
            "224.0.0.1",       // 224.0.0.0/4 multicast
            "239.255.255.255", // upper edge of multicast
            "240.0.0.1",       // 240.0.0.0/4 reserved/future
            "255.0.0.1",       // still inside the reserved range
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "203.0.114.0",
            "198.17.255.255",  // just below the benchmarking range
            "198.20.0.1",      // just above the benchmarking range
            "223.255.255.255", // just below multicast
            "192.1.1.1",       // outside 192.0.0.0/24
        ] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn private_v6_ranges_are_detected() {
        for ip in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.0.1",
            "ff02::1",           // multicast (all-nodes)
            "fec0::1",           // deprecated site-local
            "2001:db8::1",       // documentation
            "::ffff:198.18.0.1", // mapped benchmarking range
            // Non-global space the earlier hand-written deny-list admitted.
            // The first three are the dangerous ones: each names an IPv4
            // destination a suitably configured host will actually dial.
            "64:ff9b::a00:1",   // NAT64 well-known prefix -> 10.0.0.1
            "64:ff9b:1::a00:1", // NAT64 local-use prefix
            "::10.0.0.1",       // IPv4-compatible (deprecated ::/96)
            "2002:0a00:0001::", // 6to4 embedding 10.0.0.1
            "2002:7f00:0001::", // 6to4 embedding 127.0.0.1
            "100::1",           // discard-only prefix
            "2001::1",          // Teredo
            "2001:2::1",        // benchmarking
            "2001:10::1",       // deprecated ORCHID
            "2001:20::1",       // ORCHIDv2
            "3fff::1",          // documentation (RFC 9637)
            "3fff:0fff::1",     // upper edge of the documentation /20
            "5f00::1",          // SRv6 SIDs
            "0100::1",          // outside global unicast
            "1000::1",          // outside global unicast
            "4000::1",          // outside global unicast
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in [
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "2001:db9::1",   // adjacent /32, not documentation
            "2001:1::1",     // globally reachable per IANA
            "2001:4:112::1", // AS112-v6, globally reachable
            "2003::1",       // just above the 6to4 /16
            "3fff:1000::1",  // just above the documentation /20
            "2001:30::1",    // just above ORCHIDv2
            "2001:3::1",     // just above the benchmarking /48
            "2001:2:1::1",   // 2001:2::/48 is exact; this is outside it
            "3ffe::1",       // just below the documentation /20
            "2fff:ffff::1",  // upper edge of global unicast
            "2000::1",       // lower edge of global unicast
        ] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    /// A broken record beside a working one must not fail the host: a real
    /// Mastodon instance published `AAAA fe80::…` (its own interface address)
    /// next to a valid A record, and the old any-private hard-fail fed its
    /// finite failure budget until the host was blackholed.
    #[test]
    fn vet_answers_drops_private_answers_but_keeps_public_ones() {
        let public: SocketAddr = "45.147.251.24:443".parse().unwrap();
        let link_local: SocketAddr = "[fe80::216:3eff:fe2c:7747]:443".parse().unwrap();
        let vetted = vet_answers("mixed.example", vec![link_local, public], false).unwrap();
        assert_eq!(vetted, vec![public]);
    }

    #[test]
    fn vet_answers_rejects_all_private_and_empty_sets() {
        let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let link_local: SocketAddr = "[fe80::1]:443".parse().unwrap();
        let error = vet_answers("internal.example", vec![private, link_local], false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let error = vet_answers("gone.example", vec![], false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn vet_answers_allow_private_keeps_everything() {
        let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let public: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let vetted = vet_answers("dev.example", vec![private, public], true).unwrap();
        assert_eq!(vetted, vec![private, public]);
    }

    #[test]
    fn check_url_rejects_plain_http_and_ip_literals() {
        let http: Url = "http://remote.example/inbox".parse().unwrap();
        assert!(
            check_url_syntax(&http, true).is_err(),
            "http is never allowed for clearnet hosts"
        );
        let http: Url = "http://example.com/".parse().unwrap();
        assert!(
            check_url_syntax(&http, false).is_err(),
            "http is never allowed for clearnet hosts"
        );

        let private: Url = "https://10.0.0.1/inbox".parse().unwrap();
        assert!(matches!(
            check_url_syntax(&private, false),
            Err(FederationError::PrivateAddress(_))
        ));
        // allow_private waives the address check (local e2e setup)…
        assert!(check_url_syntax(&private, true).is_ok());

        for raw in [
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[fc00::1]/",
            "https://[fe80::1]/",
        ] {
            let url = raw.parse().unwrap();
            assert!(
                matches!(
                    check_url_syntax(&url, false),
                    Err(FederationError::PrivateAddress(_))
                ),
                "{raw}"
            );
            assert!(check_url_syntax(&url, true).is_ok());
        }
        let public = "https://[2606:4700::1111]/".parse().unwrap();
        assert!(check_url_syntax(&public, false).is_ok());
    }

    /// Hidden-service hosts speak plain HTTP and never resolve via DNS: the
    /// scheme gate admits them, and the resolution pre-check is skipped
    /// (`.onion`/`.i2p` would otherwise always fail NXDOMAIN here).
    #[test]
    fn check_url_admits_hidden_services_over_http() {
        for url in [
            "http://ajzihxukudausqdndsz4qmromzlo6yt5r2kqa77btk7ghxxzsbb3a4id.onion/inbox",
            "http://example.i2p/inbox",
            // An onion service behind TLS is unusual but legal.
            "https://example.onion/inbox",
        ] {
            let url: Url = url.parse().unwrap();
            check_url_syntax(&url, false).unwrap();
        }

        // The suffix must be the host's, not a clearnet look-alike's.
        let fake: Url = "http://example.onion.com/inbox".parse().unwrap();
        assert!(check_url_syntax(&fake, true).is_err());
    }
}
