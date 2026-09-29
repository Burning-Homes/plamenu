# Back up and restore

A usable Plamenu recovery set contains:

- a PostgreSQL dump;
- the media volume from the same stopped-write window;
- the matching Plamenu binary or immutable OCI image, its PostgreSQL major
  version, and the service or container configuration;
- the federation-key encryption secret version needed by that database,
  obtained from its separate escrow location.

Keep the secret-bearing `.env` and TOML configuration in a separate protected
configuration backup. Do not put the only copy of the encryption root inside
the database or media archive. Without it, restored private federation keys
cannot be used.

## Create a consistent backup

The Debian binary example stops Plamenu while PostgreSQL and media are
captured. Replace `2026-09-09` with the backup date; use a new directory each time:

```sh
{{#include ../examples/backup.sh:backup}}
```

The archive directory is created with restrictive permissions. Copy it to
independent storage. For an automated job, add an exit trap that restarts
Plamenu when an intermediate backup command fails.

For Compose, run from the deployment checkout. Replace the backup date and,
if you renamed the Compose project, the `plamenu_media-data` volume name:

```sh
{{#include ../examples/backup-compose.sh:backup}}
```

For another OCI runtime, stop the application container, dump PostgreSQL, and
snapshot or archive the persistent media volume before starting the container
again. A runtime snapshot of the container root is not a database backup.

## Restore a server

Use a replacement host with an empty database and media storage. Install the
same Plamenu version and PostgreSQL major version as the backup; attempt any
upgrade after recovery. Keep the old instance stopped when the replacement
starts, so only one server uses the federation identity.

Restore the original domains and encryption secret from the protected
configuration backup. Changing the domain does not migrate the actor and post
URLs stored in the database. Adapt host-specific paths and database addresses
if the replacement host differs.

### Debian binary installation

Follow the [binary installation guide](../start/install-binary.md) through
host preparation and release installation. Restore the saved systemd unit,
Caddy configuration, and `/etc/plamenu/plamenu.toml`, keeping the TOML readable
only by root and the `plamenu` group. Do not run the guide's startup or account
creation commands: the database must still be empty.

From the directory containing your backup, replace the example date and run:

```sh
{{#include ../examples/restore.sh:restore}}
```

### Docker Compose

On a fresh Docker host, restore `deploy/compose.yml`, `deploy/Caddyfile`,
`deploy/.env`, and `deploy/plamenu.toml`. Set `PLAMENU_IMAGE` to the backed-up
image digest, and retain the database name, user, and password from the saved
configuration. The commands below use the default database and user `plamenu`.

Run from the deployment directory, with the backup directory alongside
`deploy/`. Replace the date and, if you changed the Compose project name, the
`plamenu_media-data` volume name. These commands create fresh volumes, restore
the data, audit the federation keys, and then start the services:

```sh
{{#include ../examples/restore-compose.sh:restore}}
```

For another container runtime, restore the dump into an empty PostgreSQL
database and extract the media archive into its persistent volume. The image
runs as UID/GID `10001`; retain that ownership when restoring media. Attach
the saved configuration before starting the container.

### Check the restored server

Check `/ready` through HTTPS, then sign in with an existing account and open
old posts and their media. Test a new post, media upload, and delivery to a
remote account. Inspect worker logs and the delivery queue if anything stalls.
The restore examples run `federation keys audit` before starting the server;
resolve any reported missing or unreadable keys before proceeding.

For a rehearsal with a real server's backup, isolate the copy from public
federation and outbound mail before starting it. Use private DNS overrides for
the original domains and an isolated peer for delivery checks. A separate
Compose project alone does not prevent background jobs from reaching real
recipients.

## Repository restore drill

`./dev backup-restore` creates disposable Docker projects, posts and media,
backs up one server, destroys its volumes, and restores it. It checks existing
credentials, post identities, media bytes, pending delivery, and new federation
in both directions, then removes its own test resources.

The drill requires Bash, Docker Compose, Git, `sha256sum`, and `diff`. It builds
a native image by default. To use an existing candidate:

```sh
PLAMENU_DRILL_SKIP_BUILD=true \
PLAMENU_DRILL_IMAGE=your-candidate-image \
./dev backup-restore
```

Retain the log, script revision, and tested image identity with release evidence.
`./dev smoke-install` runs the initial installation and exchange checks only.
Both use an isolated network and local CA. Test public DNS/TLS setup and recovery
from your own backups separately.

## Native-video recovery rehearsal

The AP09 E2E rehearsal supplements the deployment drill with real native
processing and a surviving PeerTube subscriber:

```sh
e2e/.venv/bin/pytest -q -s e2e/tests/test_video_backup_restore.py
```

It requires the documented local E2E peer harness, PostgreSQL's matching
`pg_dump`/`pg_restore` major version and a free `plamenu2.local` fixture port.
It does not stop or restore the standing development server. Disposable
databases are uniquely named and created from `template0`, so an optionally
seeded developer `template1` cannot leak unrelated data into the fixture.

The test stops its writer before capturing the database and **entire** media
tree, then removes only its original disposable source directory and restores
into a new database and directory. It verifies exact files before startup,
original Video/media identities, compatibility MP4, captions, posters,
storyboard, every HLS rendition, integrity manifests and exact HTTP ranges.
Private sources remain unreadable; existing credentials, separate P2P/history
preferences, positions and private clipped playlists survive. A partly uploaded
source resumes from its committed offset, finishes native processing and refunds
its storage reservations only after acknowledged cancellation cleanup.

The matching encryption root is escrowed separately from the media/database
archive. An incorrect root fails the key audit; the correct root passes, and
the restored original signer sends a Video Update accepted by the surviving
PeerTube subscriber without changing its remote UUID.

Evidence is retained under `target/e2e/video-restore/`: mode-0700 directories,
mode-0600 recovery files/configuration, candidate and archive SHA-256 manifests,
logs and result records. These backups contain **private account data and
federation secrets**. Do not publish or upload the archives or escrow files;
apply a protected evidence/backup retention policy. Keep the original domains,
binary and PostgreSQL major version when reproducing recovery.

This is native VOD and unfinished-upload recovery evidence, not a claim that
all live-session/cache fault cases, remote storage or production disaster
scenarios have been exercised. The separately documented live crash tests and
the broader AP09 operations/release gates remain necessary.

## Retention

Keep multiple recovery points and at least one off-host copy. A synchronized
corruption, compromised credential, or operator mistake can make the newest
backup useless. Periodically restore the exact files your scheduled job
produces; “backup completed” is not evidence that the result starts.
