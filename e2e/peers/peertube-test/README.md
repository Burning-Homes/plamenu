# PeerTube reference fixture

This fixture runs the official **PeerTube 8.3.0** image, pinned by digest in
`compose.dev.yml`. `../sources.lock` pins the matching reference source used
to inspect federation contracts; the fixture itself does not build that source.

The shared fleet proxy serves `https://peertube.local`; RTMP ingest uses port
1935. See [the E2E setup](../../README.md) for machine-specific requirements.

## Upgrading existing 8.2 data

Back up the database, media, configuration and Redis state before starting
8.3 with existing volumes. Preserve the old image digest and configuration.
A rollback must restore the old database and matching file state together.
Do not simply run the old image against the upgraded database.

After the new server is healthy and its logs report schema **1125**, perform
the extra migration required by the
[8.3 release notes](https://github.com/Chocobozzz/PeerTube/releases/tag/v8.3.0):

```sh
docker compose -f compose.dev.yml exec -T -u peertube peertube \
  node dist/scripts/migrations/peertube-8.3.js
docker compose -f compose.dev.yml restart peertube
docker compose -f compose.dev.yml up -d --no-deps --wait peertube
```

This fills the video search index and migrates existing media infohashes.
Verify `/api/v1/config` reports 8.3.0, existing videos still play, and run
`tests/test_peertube_federation.py` from the configured E2E environment.
Report failures separately from instance health: a healthy upgraded PeerTube
does not establish that every Plamenu interoperability path works.

See [interoperability testing](../../../docs/INTEROPERABILITY.md) for scope;
current run results belong in the run artifacts.
