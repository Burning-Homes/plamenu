# Known limitations

Plamenu is experimental. Use a matching release's documentation and test upgrades
on restored data. There is no general production, capacity or WCAG conformance
certification. A working feature or a passing focused test does not establish release-wide
validation.

| Area | Limitation and practical response |
| --- | --- |
| Deployment | One serving process per database. Do not run a second web replica against it; optional processors are DB-less. |
| Identity and upgrades | Public/handle domains are permanent after federation. Forward migrations can prevent binary-only rollback; keep a consistent recovery set. |
| Encryption-secret rotation | Federation-key rewrap does not migrate TOTP secrets. Do not rotate the shared secret on an enrolled installation without a migration plan; see [key operations](FEDERATION_KEY_OPERATIONS.md#rotate-the-encryption-secret). |
| Private content | Private mentions are not end-to-end encrypted. Revocation cannot recall delivered copies or rewrite old backups. |
| Clients | Mastodon compatibility does not expose every native music, video or app feature. The Lemmy adapter has [explicit gaps](LEMMY_API_COMPATIBILITY.md). |
| Live notifications | The optional web setting can disrupt navigation context/scroll restoration; it defaults off. Disable it if affected. |
| Video | Native publication is public/unlisted. Browser capture depends on device/browser support; third-party media may lack playable formats or alternatives. |
| Peer delivery | HTTP playback success does not establish P2P delivery. PeerTube live exchange and cold-origin-loss redundancy have qualification limits. |
| Live performance | Prior qualification did not establish all decoded-latency/smoothness budgets, including the multi-broadcast workload. Do not advertise a guaranteed live latency or scale from those results. |
| Accessibility | Automated checks do not replace human assistive-technology verification; no site-wide WCAG 2.2 AA claim is made. |

Known qualification gaps remain open until the applicable tests pass on the
revision being shipped. Historical timings and session transcripts belong to
validation artifacts, not the operating manual. Maintainers use the
[release procedure](RELEASING.md) and [video verification guide](VIDEO_SCALE_VERIFICATION.md)
to collect current evidence.

For an account problem, contact your instance's administrators. For a software
problem, include the exact version, action, expected/actual result and redacted
logs using [Getting help](reference/getting-help.md).
