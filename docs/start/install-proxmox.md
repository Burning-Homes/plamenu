# Proxmox VE

Proxmox VE 9.1 introduced OCI image imports and application containers as a
technology preview. See the [9.1 release announcement](https://proxmox.com/en/about/company-details/press-releases/proxmox-virtual-environment-9-1)
and check the support status of your installed version. This recipe also needs
a Plamenu image version from [Releases](https://codefloe.com/plamenu/plamenu/releases).

## Import and create

In the Proxmox web interface:

1. Open a storage that accepts container templates and choose **Pull from OCI
   registry**.
2. Pull the immutable `codefloe.com/plamenu/plamenu@sha256:…` reference from
   the release assets.
3. Create an unprivileged container from that imported template. Keep its
   image entrypoint, allocate a fixed private address, and enable start at boot.
4. Add persistent storage at `/var/lib/plamenu/media` and make it writable by
   UID/GID 10001. Put the secret-bearing configuration at
   `/etc/plamenu/plamenu.toml`, readable by that UID.
5. Give `/tmp` at least 1 GiB of writable space, connect the container to a
   private PostgreSQL service, and reverse proxy private port 8420 over HTTPS.

If the first boot stops with `Network unreachable`, delay the image entrypoint
until the container has a default route, as in the Incus guide. Proxmox exposes
this as the container `entrypoint` setting; keep the final command exactly
`plamenu --config /etc/plamenu/plamenu.toml serve`.

Apply every item in the [OCI runtime requirements](install-oci.md#runtime-requirements),
including the wildcard Webxdc origin and narrow `trusted_proxies` value.

The import squashes OCI layers into the container root filesystem. An
upgrade is therefore a stopped replacement, not an in-place layer swap: back
up PostgreSQL and media, import the new digest, create a replacement container,
reattach the preserved media/configuration, then verify `/ready` before
retiring the old container. Keep the old recovery set until the new instance
has passed a restore rehearsal.

The authoritative workflow is in the current
[Proxmox VE administration guide](https://pve.proxmox.com/pve-docs/chapter-pct.html#pct_container_images),
under **Container Images → Open Container Initiative (OCI) Images**. Storage,
network, backup, and cluster commands vary by Proxmox layout, so Plamenu does
not prescribe volume IDs or bridge names.
