# Install a new server

Download signed AMD64 or ARM64 archives from
[Releases](https://codefloe.com/plamenu/plamenu/releases), or use the
[multi-architecture OCI image](install-oci.md). The release page includes
checksums and signature-verification instructions. For local evaluation from
source, use the [development guide](../DEVELOPMENT.md).

| Path | Host | Deployment setup |
| --- | --- | --- |
| [Debian binary](install-binary.md) | A new or existing Linux server | Linux binary, systemd unit, host configuration, and Caddy |
| [OCI image](install-oci.md) | Existing container infrastructure | Image running as UID 10001, with configuration and storage requirements |

Container examples cover [Docker Compose](install-compose.md),
[Incus](install-incus.md), and [Proxmox VE](install-proxmox.md).

## Requirements

You need a stable public domain, DNS control, ports 80 and 443, PostgreSQL,
space for media and independent backups, and an SMTP relay if the server will
send registration or password-reset mail. Webxdc also needs DNS for both
`webxdc.<social-domain>` and `*.webxdc.<social-domain>`.

Choose `domain` and optional `account_domain` permanently before federation
begins. If handles should use an apex domain while Plamenu runs on a subdomain,
read [Split-domain handles](../operators/configuration.md#split-domain-handles)
before installing.

After either installation path, [create the first administrator](first-administrator.md),
reboot once, rehearse a restore, and configure external checks for `/ready` and
disk space before inviting members.
