# Operate a server

For initial setup, follow [installation](../start/install.md) and
[create the first administrator](../start/first-administrator.md).

| Task | Guide |
| --- | --- |
| Set domains, database, storage, mail, and proxy settings | [Configuration](configuration.md) |
| Manage registration, accounts, reports, and server policy | [Administration](administration.md) |
| Change the running version | [Upgrading](upgrading.md) |
| Preserve and recover data | [Backup and restore](backup-and-restore.md) |
| Watch readiness, resources, and delivery | [Monitoring](monitoring.md) |
| Diagnose failures | [Troubleshooting](troubleshooting.md) |

Run one serving process per database. Keep the public and handle domains
unchanged after federation begins. Back up PostgreSQL and media together while
writes are stopped, and preserve the matching encryption secret separately.

Specialized guides cover [background queues](../QUEUES.md),
[federation keys](../FEDERATION_KEY_OPERATIONS.md),
[remote profile history](../REMOTE_HISTORY_OPERATOR_RUNBOOK.md), and
[Tor/I2P routing](../TOR_TRANSPORT.md).
