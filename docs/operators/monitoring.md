# Monitor the server

Monitor public HTTPS readiness, resource use, delivery queues, and backups.

## Probes

- `GET /health` is liveness: the process answers.
- `GET /ready` checks PostgreSQL and worker-supervisor health. Use it for
  external uptime alerts, container health, and traffic admission.

Probe `/ready` through the public HTTPS origin so DNS, Caddy, TLS, the backend,
and PostgreSQL are all exercised. Also retain a local probe to distinguish edge
failure from application failure.

## Alert on

- sustained `/ready` failure or elevated HTTP 5xx responses;
- repeated service/container restarts or worker crash messages;
- PostgreSQL connection exhaustion, slow queries, and database storage;
- media and temporary-transcode free space;
- outbound delivery queue age, due work, and large per-host backlogs;
- mail queue failure when account recovery depends on e-mail;
- backup age and the last successful restore rehearsal;
- certificate expiry and DNS changes for both the social and Webxdc hosts.

## Logs and queue inspection

Plamenu writes structured text to stdout and stderr. Set `RUST_LOG` in the
deployment environment to adjust filtering; avoid debug logging indefinitely
on a busy server because URLs and operational context can be sensitive.

On the binary deployment, follow logs with `journalctl -u plamenu -f`. Compose
uses `docker compose … logs -f plamenu`; Incus uses
`incus console plamenu --show-log`. Use the equivalent facility for another
runtime.

Inspect outbound work without changing it:

```bash
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation queue inspect
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation reachability
```

The [queue guide](../QUEUES.md) explains retry, leasing, host fairness, and the
few operations with different guarantees.

## Capacity signals

No general minimum hardware requirement has been established. Measure CPU,
memory, database growth, and media storage under the workload you expect.
Video processing and account archives also need temporary space. Keep room
for a restore and alert before storage is exhausted.

Increase the default database pool only after observing pool waits and checking
PostgreSQL memory and connection headroom. CPU saturation in password hashing
or media processing is not fixed by adding database connections.
