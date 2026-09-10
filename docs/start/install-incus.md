# Incus

Use Incus 7.0 LTS or a newer supported release; it can consume OCI images as
application containers. This recipe assumes an existing bridge and storage
pool, an external PostgreSQL database, a fixed container address, and a reverse
proxy managed outside the Plamenu container.

Choose the Plamenu image version from [Releases](https://codefloe.com/plamenu/plamenu/releases).

The commands use `default` as the storage pool, `10.0.0.42` as the container
address, and `/root/plamenu.toml` as the configuration file. Replace these
values for your installation.

## Prepare configuration and storage

Create `/root/plamenu.toml` on the Incus host from
`deploy/plamenu.toml.example`. Use the database's private address, bind
`0.0.0.0:8420`, and trust only the reverse proxy.

Create separate configuration and media volumes with the image's UID/GID:

```sh
{{#include ../examples/install-incus.sh:storage}}
```

## Create the application container

Replace `REPLACE_WITH_RELEASE_DIGEST` with the digest from the release assets,
keeping the `sha256:` prefix:

```sh
{{#include ../examples/install-incus.sh:instance}}
```

The route wait is intentional: an Incus application process can start before
its default network route appears. Keep port 8420 private and point the host or
edge reverse proxy at it. The host proxy must also serve the wildcard Webxdc
origin described in `deploy/Caddyfile`.

Back up the PostgreSQL database and both custom volumes as one recovery set.
To upgrade, stop the instance and recreate its root from the new digest while
reattaching the same configuration and media volumes. Do not delete the custom
volumes with the old instance.

See the upstream [Incus OCI remote guide](https://linuxcontainers.org/incus/docs/main/howto/images_remote/)
and [custom-volume documentation](https://linuxcontainers.org/incus/docs/main/howto/storage_volumes/)
for registry authentication, clusters, and storage-specific behavior.
