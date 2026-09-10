# Deployment examples

The Debian host example binds only to loopback and uses PostgreSQL peer
authentication over a local socket:

```toml
{{#include ../../deploy/plamenu.host.toml.example}}
```

The OCI/Compose example binds to the container network and reaches PostgreSQL
by service name:

```toml
{{#include ../../deploy/plamenu.toml.example}}
```

Copy the example matching the deployment, replace every placeholder, and
protect the result from other users. Plamenu's tests parse both exact files
with the real configuration loader.

For every available setting and current default, run `plamenu config generate`
with the release binary. See [configuration](../operators/configuration.md) for
the decisions that must be made before federation begins.
