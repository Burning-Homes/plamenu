# Install the binary on Debian

This is the reference deployment: Debian 13, the release binary, PostgreSQL 17
from Debian, Caddy, and systemd. Releases include AMD64 and ARM64 binaries.
The static binaries can run on other Linux distributions of the same architecture; adapt the package
names and service setup for your host.

The steps assume a fresh, dedicated server and an administrator with `sudo`.
Run the commands section by section, replacing the example values for your server.

## Prepare the host

```sh
{{#include ../examples/install.sh:host}}
```

The operating-system user and PostgreSQL role have the same name. Debian's
default peer authentication therefore lets Plamenu use the local database
socket without storing a database password or opening PostgreSQL to the
network.

## Install a release

Choose a version from [Releases](https://codefloe.com/plamenu/plamenu/releases)
and follow its verification instructions. Replace `0.6.0` with the selected version
in the `version` variable. The commands select the host architecture:

```sh
{{#include ../examples/install.sh:release}}
```

`plamenu-VERSION-linux-ARCH.tar.gz` contains one executable, the systemd unit,
example Plamenu and Caddy configurations, and the license.

## Configure the server

```sh
{{#include ../examples/install.sh:configure}}
```

In `/etc/plamenu/plamenu.toml`, replace `social.example.com` and the encryption
secret. Keep the socket database URL and loopback bind for this single-host
layout. Configure SMTP before enabling a mail-dependent account flow.

In `/etc/caddy/Caddyfile`, replace every `social.example.com`. Point the social
domain, `webxdc.<social-domain>`, and `*.webxdc.<social-domain>` to the server.
The [configuration guide](../operators/configuration.md) explains split-domain
handles and why proxy trust must remain narrow.

## Validate and start

```sh
{{#include ../examples/install.sh:start}}
```

`/health` checks that the process answers. `/ready` also checks PostgreSQL and
worker-supervisor health. If the public check fails, inspect
`journalctl -u plamenu` and use the
[troubleshooting guide](../operators/troubleshooting.md).

Next, [create the first administrator](first-administrator.md).
