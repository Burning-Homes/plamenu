# Troubleshoot

Record the symptom and where it occurs. Save
timestamps, the public URL or handle, local account, remote host, HTTP status,
and relevant log lines before restarting or deleting anything.

## The site does not become ready

1. Check `systemctl status plamenu postgresql` or the equivalent container and
   database status in your OCI runtime.
2. Inspect Plamenu and PostgreSQL logs.
3. Request `/health` and `/ready` locally, then through public HTTPS.
4. Verify `database_url`, database authentication, and media/scratch free space.
5. Check whether another Plamenu process holds the single-writer database lock.
6. Confirm the reverse proxy can reach port 8420 and that `trusted_proxies`
   exactly matches the actual proxy path.

Do not treat liveness success with readiness failure as healthy; it commonly
points to PostgreSQL or a worker supervisor.

## Writer-lock errors

Plamenu permits one serving process and one CLI command per database. If startup
reports a held writer lock, check for another service, container, or CLI command
using that database. Let the active command finish or stop the identified
duplicate process. These are session locks: they disappear when the owning
database session ends and do not require deleting a lock file or database row.

The original lock connection is monitored throughout initialization and
operation. A connection failure or five-second heartbeat timeout makes the
process exit; it does not reconnect and continue under an unverified lock.
Correlate the exit with PostgreSQL restarts, connection termination, and network
failures. Before retrying an interrupted mutating CLI command, inspect whether
its action already committed. See the [deployment boundary](../ARCHITECTURE.md#deployment-boundary)
for the limits of this guard.

## Sign-in or e-mail fails

Check the exact username/e-mail, server time, SMTP configuration, relay logs,
and whether the flow requires confirmed mail. For an existing account, an
authorized operator can set new credentials with `plamenu account passwd`.
Never ask a member to send their old password or TOTP secret.

## A remote account or post cannot be found

Try the exact handle and then the canonical public URL. Use read-only diagnosis:

```bash
plamenu federation webfinger user@example.com
plamenu federation fetch https://example.com/@user
plamenu federation reachability
```

Check DNS, TLS, WebFinger links, HTTP signatures, authorized fetch, local and
remote domain policy, and whether the destination resolves to a forbidden
private address. Search is not a global crawl and will not reveal private or
unavailable objects.

## Federation delivery is delayed

Run `plamenu federation queue inspect`, then correlate the oldest due jobs with
reachability output and logs. A growing global queue suggests local database,
network, or worker trouble; one remote host with backoff more often indicates a
peer-specific or policy problem. Read [background queues](../QUEUES.md) before
altering state.

## Media fails

Check persistent media and `/tmp` free space, file permissions, FFmpeg logs,
configured limits, and whether the failure affects uploads, remote cache, or
only transcoding. `plamenu media reconcile` reports orphan candidates without
deleting them. Use `--delete` only during low upload activity after reviewing
the report and taking a backup.

## After an upgrade

Compare the running version and binary checksum or image digest with the
intended release, then read its migration and configuration notes again. Do
not start an older binary
against a migrated database unless the release explicitly permits it. Follow
the [rollback procedure](upgrading.md#rollback).

## Before asking for help

Include the Plamenu version and full build identity, deployment method,
architecture, redacted configuration relevant to the symptom, reproduction
steps, timestamps, and a short log excerpt. Remove passwords, tokens, cookies,
private keys, encryption roots, private post content, and member e-mail
addresses. Report security vulnerabilities through the private process in the
repository's `SECURITY.md`.
