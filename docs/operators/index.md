# Operate a server

For initial setup, follow [installation](../start/install.md) and
[create the first administrator](../start/first-administrator.md).

| Task | Guide |
| --- | --- |
| Set domains, database, storage, mail, and proxy settings | [Configuration](configuration.md) |
| Manage registration, accounts, reports, and server policy | [Administration](administration.md) |
| Manage discovery sources and history | [Discovery](discovery.md) |
| Control storage, processing and mounted music | [Media operations](media.md) |
| Change the running version | [Upgrading](upgrading.md) |
| Preserve and recover data | [Backup and restore](backup-and-restore.md) |
| Watch readiness, resources, and delivery | [Monitoring](monitoring.md) |
| Diagnose failures | [Troubleshooting](troubleshooting.md) |

## Responsibility and first-day checks

Instance administrators manage accounts, reports, rules and policy through the
web console. Hosting operators manage the executable/container, database,
filesystem, proxy, network and secrets. One person may do both; a moderator role
does not grant host access.

Before inviting members, verify public HTTPS readiness, owner and ordinary
sign-in, password-reset mail, a post/upload, remote account resolution and a
restore rehearsal. If enabling music, video or apps, exercise their actual
upload/playback or app-launch path too: a healthy home page does not test media
processing, Webxdc DNS/TLS or encoder reachability.

Keep alerts for readiness, free disk space, queued failures and backup age;
review reports and failed jobs regularly. Record the exact installed version
and compare release notes before changing it.

Run one serving process per database. Keep the public and handle domains
unchanged after federation begins. Back up PostgreSQL and media together while
writes are stopped, and preserve the matching encryption secret separately.

Specialized guides cover [background queues](../QUEUES.md),
[federation keys](../FEDERATION_KEY_OPERATIONS.md),
[remote profile history](../REMOTE_HISTORY_OPERATOR_RUNBOOK.md), and
[Tor/I2P routing](../TOR_TRANSPORT.md).
