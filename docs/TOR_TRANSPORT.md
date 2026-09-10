# Hidden-service transport

Plamenu can route outbound federation and media requests to `.onion` and `.i2p`
destinations through an external SOCKS5 proxy. Run Tor or an I2P proxy
separately; the server keeps its existing ActivityPub identity and federation
policy. This configuration covers outbound connections.

## Configuration

Prefer per-network routes so clearnet requests continue through Plamenu's local
DNS/IP destination checks:

```toml
[federation]
onion_proxy_url = "socks5h://127.0.0.1:9050"
i2p_proxy_url = "socks5h://127.0.0.1:4447"
no_proxy = ["example.com"]
```

Use `socks5h://`, not `socks5://`: the `h` delegates hidden-name resolution to
the proxy. Plamenu rejects the local-resolution form.

A global `proxy_url` routes clearnet names through the proxy too. When
`allow_private_fetch=false`, configuration is rejected unless
`trust_proxy_destination_filtering=true`. Set that acknowledgement only when
the proxy independently blocks loopback, private/link-local ranges, carrier
grade NAT, cloud metadata endpoints, and DNS rebinding.

Hidden-service URLs may use plain HTTP because overlay transport supplies the
secure channel. Plain HTTP remains rejected for ordinary hosts. `no_proxy`
takes precedence over all proxy routes.

## Runtime behavior

- WebFinger, actor/object fetches, delivery, and remote media use the same
  destination-aware client selection.
- Onion deliveries and fetches are serialized because concurrent Tor requests
  are failure-prone. Clearnet work retains normal concurrency.
- Queue retry and backoff behave exactly as for clearnet failures. A missing or
  unavailable proxy fails cleanly; it does not fall back to direct DNS.
- Reachability state is separated by transport, so an onion failure does not
  mark the same clearnet host unreachable.

The live local acceptance test is `e2e/tests/test_tor_federation.py`. It covers
discovery, follow, signed delivery, remote media, and the absence of direct
`.onion` DNS traffic. Run it through the E2E environment documented in
[DEVELOPMENT.md](DEVELOPMENT.md).
