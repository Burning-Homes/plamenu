# Configuration

Plamenu reads deployment settings from a TOML file supplied with `--config`.
The reference binary and OCI deployments both use
`/etc/plamenu/plamenu.toml`. Generate the fully commented template for the
exact release with:

```bash
plamenu --config plamenu.toml config generate
```

Use the [host or OCI example](../reference/deployment-config.md) as a starting
point. The configuration tests parse both files.

## Settings that need an early decision

`domain`
: Permanent HTTPS host used in ActivityPub actor and object identifiers.

`account_domain`
: Optional permanent handle domain. It must equal `domain` or be a parent of
  it. Omitting it uses `domain`.

`database_url`
: PostgreSQL connection string. The Debian host example uses local peer
  authentication and stores no password. The Compose example's password must
  match `POSTGRES_PASSWORD` in `deploy/.env`.

`encryption_secret` and `encryption_secret_version`
: Root used to encrypt federation private keys and TOTP secrets. Generate a strong independent
  value, restrict file access, and escrow it outside database and media
  backups. Follow the [key runbook](../FEDERATION_KEY_OPERATIONS.md) to rotate
  it; do not simply replace the value. That guide documents the current TOTP
  migration limitation.

`trusted_proxies`
: Exact proxy addresses or tight subnets whose forwarding headers are trusted.
  A broad private network lets unrelated machines forge the client address
  used by security logs, blocking, and rate limits.

`media_dir`
: Persistent local media root. It must be writable by Plamenu and included in
  coordinated backups.

`video_analytics`
: Optional root-level boolean (default `true`) allowing privacy-preserving
  delivery analytics. Each channel still defaults to **off** until its owner
  opts in. Set `false` to stop collection server-wide on restart and remove
  retained aggregates in bounded maintenance. No viewer identifiers are stored.
  See [Private creator delivery analytics](../members/channels.md).

## Split-domain handles

To host at `social.example.com` while publishing `alice@example.com`, set both
values before creating accounts:

```toml
domain = "social.example.com"
account_domain = "example.com"
```

The web server at `example.com` must preserve the complete request URI while
redirecting WebFinger, host-meta, and NodeInfo discovery to the hosting domain:

```caddyfile
example.com {
	redir /.well-known/webfinger* https://social.example.com{uri} permanent
	redir /.well-known/host-meta* https://social.example.com{uri} permanent
	redir /.well-known/nodeinfo* https://social.example.com{uri} permanent
}
```

Do not proxy the Plamenu API or sign-in pages through the account domain.

## Mail

SMTP is optional for a closed CLI-created installation, but registration,
confirmation, password reset, and other mail-dependent workflows need it.
Configure the relay, restart Plamenu, and test delivery before allowing those
workflows. Keep relay credentials out of version control and limit the sender's
permissions at the provider.

## Optional services

| Section or key | Configure when… |
| --- | --- |
| `[translation]` | Members should translate posts via DeepL, LibreTranslate or an OpenAI-compatible service. Choose its endpoint/model/languages; content is sent to that provider. |
| `[notifications]` | Change seen-notification retention (default 90 days) and UTC purge/digest hours (03:00/08:00). Unseen notifications are preserved; mail delivery can repeat after a crash. |
| `[federation]` | Route guarded outbound federation through a proxy or hidden-service transport; see [Tor/I2P](../TOR_TRANSPORT.md). |
| `conversation_containers` | Opt into hosting experimental private conversation containers. Default is off. |
| `update_check_url` | Poll a selected JSON release feed for update notices. This does not automatically install a release. |
| `csp_reporting` | Collect CSP reports. They may contain sensitive URLs; plan log access/retention. |

The generated template contains accepted fields and defaults. Unknown fields
are rejected. Keep provider credentials outside version control. Do not assume
federation proxy settings also route translation services or webhooks.

## Native video storage and processing

`[native_video]` defaults include a 2 GiB source ceiling, 50 GiB per account,
1 TiB global storage, 8 GiB pending storage, 32 unfinished drafts, two processing
slots and a 1 GiB disk floor. Processing can reserve much more than the source:
it must admit retained originals, output qualities and scratch before work starts.
Avoid tuning from the final film's size alone.

For music, `[music]` controls storage, role/instance bitrate ceilings, renditions
and download jobs. Its default retained-audio budgets are 50 GiB per account
and 1 TiB overall. Read [media operations](media.md) before enabling paired
storage, remote video copies, processors or mounted libraries.

## Live ingest

`live_ingest_bind` controls plaintext RTMP, loopback by default. `[live_rtmps]`
provides a separate TLS listener with certificate/key paths. Configure the
actual encoder path, firewall and certificates before advertising it to members.
The website's HTTPS reverse proxy does not automatically expose an RTMP port.
Keep stream credentials private. See [live broadcasting](../members/live.md).

## Apply a change

Protect the TOML file so only administrators and the service account can read
its secrets. Restart for file-based configuration changes, inspect startup logs
and `/ready`, then test the affected workflow. UI-managed instance policy is
stored in PostgreSQL and generally takes effect without a process restart.

Generate templates into a **new file** for comparison; do not overwrite a live
configuration to pick up defaults. Keep permanent identity values and use the
[key procedure](../FEDERATION_KEY_OPERATIONS.md) for secret rotation. Maintain an
inventory of the release, image/binary checksum, proxy config, external mounts,
mail/translation services and independent secret escrow.
