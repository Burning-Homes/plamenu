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
: Root used to encrypt federation private keys. Generate a strong independent
  value, restrict file access, and escrow it outside database and media
  backups. Follow the [key runbook](../FEDERATION_KEY_OPERATIONS.md) to rotate
  it; do not simply replace the value.

`trusted_proxies`
: Exact proxy addresses or tight subnets whose forwarding headers are trusted.
  A broad private network lets unrelated machines forge the client address
  used by security logs, blocking, and rate limits.

`media_dir`
: Persistent local media root. It must be writable by Plamenu and included in
  coordinated backups.

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

## Federation fetch and proxies

Authorized fetch and remote transport affect interoperability and privacy.
Route `.onion` or `.i2p` destinations with their dedicated proxy settings so
ordinary clearnet fetches retain Plamenu's DNS/IP destination checks. A global
proxy is an advanced deployment that requires its own egress controls. Read the
[Tor and I2P runbook](../TOR_TRANSPORT.md) before enabling it.

## What belongs in the admin interface

Use the admin interface for registration, retention, media limits, federation
policy, trends, and moderation settings. The TOML file configures identity,
networking, database, storage, encryption, mail transport, and outbound routing.
