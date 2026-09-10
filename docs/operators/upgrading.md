# Upgrade

Experimental releases can change the database or configuration incompatibly.
Check the release notes for an upgrade path from your version. If none is
documented, test on a restored copy before changing the live server.

## Before changing the binary or image

1. Record the exact Plamenu version and binary checksum or running
   `image@sha256:…`, configuration, PostgreSQL version, and free space.
2. Read the release notes for migrations, configuration changes, removals, and
   post-upgrade work.
3. Run `plamenu federation keys audit` with the current binary.
4. Make a coordinated stopped-write database and media backup. Verify that the
   matching encryption secret is independently available.
5. Test the restore and upgrade on a separate host or isolated project when the
   release changes storage, identity, or federation behavior.

## Upgrade a binary installation

Download `plamenu-VERSION-linux-amd64.tar.gz` or
`plamenu-VERSION-linux-arm64.tar.gz` for your host and the selected release,
then extract it into a new directory. The archive contains `plamenu` at its root. From that directory,
after the backup completes:

```bash
sudo systemctl stop plamenu
sudo install -m 0755 ./plamenu /usr/local/bin/plamenu.new
sudo mv /usr/local/bin/plamenu /usr/local/bin/plamenu.previous
sudo mv /usr/local/bin/plamenu.new /usr/local/bin/plamenu
sudo systemctl start plamenu
sudo journalctl -u plamenu -f
```

Keep `plamenu.previous` only as provenance and a recovery aid. Do not run it
against a database already migrated by the new version unless the release notes
explicitly allow that.

## Upgrade Docker Compose

Update `PLAMENU_IMAGE` to the release's immutable digest, pull it, and recreate
only the Plamenu service:

```bash
docker compose --env-file deploy/.env -f deploy/compose.yml pull plamenu
docker compose --env-file deploy/.env -f deploy/compose.yml up -d plamenu
docker compose --env-file deploy/.env -f deploy/compose.yml logs -f plamenu
```

For Incus, Proxmox VE, or another OCI runtime, create a replacement root from
the new digest and reattach the preserved configuration and media storage. Do
not delete the preserved volumes. Forward migrations run before the server
accepts traffic.

In another terminal, wait for the public `/ready` endpoint. Then test owner and
ordinary sign-in, a post with media, local search, e-mail, a remote follow or
mention, queue health, and the administration pages affected by the release.

## Rollback

Do not point an older binary at a database after a forward migration unless the
release instructions explicitly say that is safe. A rollback normally means
restoring the pre-upgrade PostgreSQL and media pair, restoring the matching
configuration and encryption secret, and starting the previous binary or
immutable image.
That discards writes accepted after the backup, so make the decision before
reopening the server widely.

Retain the pre-upgrade recovery set until the new version has run through a
normal workload and at least one fresh backup has passed a restore test.
