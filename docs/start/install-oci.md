# Run the OCI image

Each release publishes one OCI image tag containing `linux/amd64` and
`linux/arm64`. The release page links to the container package and gives the
pull command and immutable digest. Docker selects the host's architecture:

```sh
docker pull codefloe.com/plamenu/plamenu:v0.6.0
```

Replace the version with an available release. Private packages require
`docker login codefloe.com`. Use the published `image@sha256:…` reference for
deployments that must stay on that exact release. Local preparation also retains
both tested images; see [release preparation](../RELEASING.md).

## Runtime requirements

An OCI deployment must provide all of the following:

| Concern | Requirement |
| --- | --- |
| Process | The image entrypoint runs `plamenu --config /etc/plamenu/plamenu.toml serve` as UID/GID 10001 |
| Configuration | Read-only file at `/etc/plamenu/plamenu.toml`, readable by UID 10001 |
| Media | Persistent, writable storage at `/var/lib/plamenu/media`, owned by UID/GID 10001 |
| Scratch | Writable `/tmp`; allow at least 1 GiB for large media operations |
| Database | A supported PostgreSQL database reachable from the container; do not publish PostgreSQL publicly |
| HTTP | Container port 8420 behind an HTTPS reverse proxy |
| Health | `/health` for liveness and `/ready` for database-aware readiness |
| Identity | One serving Plamenu process per database; permanent `domain` and `account_domain` |
| Proxy trust | `trusted_proxies` contains only the actual proxy addresses or subnet |

The root filesystem can be read-only. Drop every Linux capability and set
`no-new-privileges` when the runtime supports it. Plamenu shells out to the
FFmpeg and FFprobe already present in the official image.

## Pick a runtime

- [Docker Compose](install-compose.md) is the supplied all-in-one example and
  includes PostgreSQL and Caddy.
- [Incus](install-incus.md) runs the same image as a native Incus application
  container; the guide was exercised against a real Incus deployment.
- [Proxmox VE](install-proxmox.md) can import the same image through its native
  OCI support, currently a Proxmox technology preview.

Other OCI runtimes need the same process, storage, database, and proxy setup.
This repository does not supply Kubernetes, Podman, or Nomad deployment recipes.
