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

`video_analytics`
: Optional root-level boolean (default `true`) allowing privacy-preserving
  delivery analytics. Each channel still defaults to **off** until its owner
  opts in. Set `false` to stop collection server-wide on restart and remove
  retained aggregates in bounded maintenance. No viewer identifiers are stored.
  See [Private creator delivery analytics](../VIDEO_ANALYTICS.md).

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

## Optional encrypted live encoder ingest

Live broadcasting works without another service: Plamenu accepts local
plaintext RTMP and HTTPS-FLV through its normal HTTPS origin. For encoders that
require the RTMPS protocol, opt in to Plamenu's own TLS listener:

```toml
live_ingest_bind = "127.0.0.1:1935"
[live_rtmps]
bind = "0.0.0.0:1936"
cert = "/etc/letsencrypt/live/social.example.com/fullchain.pem"
key = "/etc/letsencrypt/live/social.example.com/privkey.pem"
```

Use a certificate valid for `domain`, grant the Plamenu process read access to
the certificate and key, and allow the RTMPS port through the host firewall.
Plamenu checks the certificate/key at startup and reloads them for new
connections within a minute of renewal. Existing broadcasts keep running.
The listener forwards decrypted RTMP to the local native pipeline; it does not
spawn a proxy or a separate transcoder. Keep the plaintext listener on loopback
unless a trusted local network genuinely requires it. Without `[live_rtmps]`,
no RTMPS port is opened and installation steps do not change.

## Native Video storage and processing

Native uploads use Plamenu's own durable database queue, media store and
existing encoder runtime. No companion server or additional install step is
introduced. Defaults can be overridden in TOML:

```toml
[native_video]
max_upload_mib = 2048
per_account_gib = 50
global_gib = 1024
pending_gib = 8
unfinished_drafts = 32
jobs = 2
min_free_gib = 1
```

Quotas count retained originals and all journaled output revisions. A file
scheduled for deletion stays charged until the storage worker acknowledges
removal. A restart recovers committed upload offsets, source reservations and
generation-fenced processing leases. Failed encoding retains the source for
retry; unfinished drafts expire, but published media is not draft-expired.

Worker slots are bounded before a job is claimed. Temporary-space reservations
cover the source spool, original and compatibility MP4 outputs, at most six
HLS files and possible destination copies, poster scratch and a 2 MiB
storyboard plus its copy. For a
default-ceiling 2 GiB source, this conservative reservation is about 34 GiB
plus 16 MiB per active job, in addition to the free-space floor. Actual outputs
are often much smaller. Insufficient free space leaves work queued; it does
not delete the original or relax the output ceilings. Other processes can use
the same filesystem, so monitor real free space too. Do not place large-video
test/production media on a small RAM-backed `/tmp` mount.

Uploader turns persist in PostgreSQL. Eligible uploaders with fewer active
jobs and older turns take precedence, retaining FIFO within an uploader.
Only the short claim/turn transaction is serialized; encoding holds no queue
transaction. One account's older bulk uploads therefore cannot monopolize
both free slots while another eligible account waits. Account suspension and
lease/generation fences continue to apply.

Native jobs also bound FFmpeg decoder/encoder thread shares to half the
available CPUs divided by the configured worker slots (at least one, at most
eight threads per decoder/encoder); filter execution uses one thread. Lower
renditions are sequential, not a second unbounded pool. Increasing `jobs`
therefore changes CPU shares as well as disk admission; measure the actual
workload before raising it.

The normal installation remains a single native media directory. An optional
`[media_mirror]` pairs it with a second existing disk directory, with serialized
bandwidth and free-space controls and acknowledged deletion on both disks.
Read [Native media storage and replication](../VIDEO_STORAGE.md) before
enabling it: completed media and upload prefixes are mirrored, but active
live/DVR scratch is not, and replication does not replace a consistent backup.

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
