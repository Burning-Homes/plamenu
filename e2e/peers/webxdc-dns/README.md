# Wildcard DNS for Webxdc browser development

On a Linux development host with systemd-resolved, dnsmasq and iproute2 installed:

```sh
sudo ./e2e/peers/webxdc-dns/install.sh
```

This installs a boot-persistent DNS service on `192.0.2.53:5354` on a dedicated
`plamenu-dns` dummy link (a locally assigned documentation-range address).
systemd-resolved routes only `webxdc.plamenu.local`,
`webxdc.plamenu2.local` and their subdomains to it. All resolve to `127.0.0.1`;
other query types receive an empty response. Normal network DNS remains on its
existing links. `/etc/hosts` cannot express these wildcard mappings.

The E2E Caddy configuration already serves the corresponding wildcard TLS
origins. Start Caddy and the two Plamenu servers separately. The host browser
must trust the E2E Caddy CA and use the system resolver for these development
names. Existing session-specific `/etc/hosts` entries are no longer necessary.

To remove this setup:

```sh
sudo systemctl disable --now plamenu-webxdc-dns.service
sudo rm /etc/systemd/system/plamenu-webxdc-dns.service /etc/plamenu-webxdc-dns.conf
sudo systemctl daemon-reload
sudo resolvectl flush-caches
```

Reference: [dnsmasq domain address rules](https://dnsmasq.org/docs/dnsmasq-man.html).
