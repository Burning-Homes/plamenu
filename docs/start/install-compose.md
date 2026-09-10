# Docker Compose

The supplied Compose stack runs PostgreSQL, Plamenu, and Caddy. It requires
Docker Engine, Compose, and a release image. Choose a version from
[Releases](https://codefloe.com/plamenu/plamenu/releases).

## Prepare the deployment

Replace `v0.6.0` with the selected release tag and copy its examples:

```sh
{{#include ../examples/install-compose.sh:prepare}}
```

Before starting:

1. Put the release's immutable `image@sha256:…` reference in `deploy/.env`.
2. Replace the PostgreSQL password in `.env` and `plamenu.toml` with the same
   random value.
3. Set the social and Webxdc domains.
4. Set `encryption_secret` to `openssl rand -hex 32` and escrow it separately
   from PostgreSQL and media backups.
5. If `172.30.0.0/24` overlaps another network, change it in both files.
6. Configure SMTP before enabling registration or password reset.

## Validate and start

```sh
{{#include ../examples/install-compose.sh:start}}
```

All three services should become healthy and the public `/ready` request must
succeed. Continue with [the first administrator](first-administrator.md), using
the OCI command shown there.
