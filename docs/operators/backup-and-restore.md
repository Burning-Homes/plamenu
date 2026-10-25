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

## Media and interrupted work

Capture the **whole** primary media root, including private drafts, retained video
sources, live scratch and storage markers. For paired storage, preserve matching
roots/markers from the same stopped-write recovery point; never mix generations.
An asynchronous live checkpoint can lag and is not a backup. Mounted music
source roots are outside `media_dir`: include them in your recovery plan and
restore their expected mount identity before enabling scans or source writes.

The origin owns private processor recovery; worker scratch is disposable.
After restore, inspect pending processing, live recording finalization and media
cleanup. A previous encoder must not appear on air merely because its waiting
publication exists. Verify a recovered recording and a new broadcast separately.
See [media operations](media.md) for mirror and processor boundaries.

## Rehearse and retain

Regularly restore a real recovery set into an isolated environment with outbound
federation and mail blocked. A separate container project alone does not stop
outgoing requests. Keep the live server as the only public owner of its identity.
Check readiness, key decryption, accounts, old media, source mounts and queued
work before accepting the recovery procedure.

Keep multiple dated recovery points, at least one off-host copy, and a tested
way to recover secrets independently. Protect backups as private account data;
account deletion does not rewrite older backups. Record version, PostgreSQL
major, timestamp, checksums, restoration result and the retention/expiry policy.
