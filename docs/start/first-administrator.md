# Create the first administrator

The web registration flow cannot bootstrap its own administrator. Create the
first local account with the CLI, then assign the built-in `Owner` role.

Replace `alice`, `alice@example.com`, and `replace-with-a-unique-password`
with the owner's username, email address, and password:

```sh
{{#include ../examples/install.sh:first-admin}}
```

Use the same username in both commands. Role names are case-insensitive.
To inspect the roles available in this release, run:

```sh
sudo -u plamenu /usr/local/bin/plamenu \
  --config /etc/plamenu/plamenu.toml role list
```

For the Compose deployment, run these commands from the deployment checkout:

```sh
{{#include ../examples/install-compose.sh:first-admin}}
```

On another OCI runtime, execute the same two `plamenu --config …` commands as
UID 10001 in the running application container.

Open the configured HTTPS domain and sign in. The **Admin** link should now be
available in the main navigation. Use it to set the server name and description,
registration policy, server rules, moderation defaults, retention, and mail
behavior before inviting members.

Do day-to-day browsing from a separate ordinary account when practical. Keep
the owner account protected with a unique password, two-factor authentication,
and a recovery path tested by another authorized operator.

All other CLI commands and current flags are in the generated
[command-line reference](../reference/cli.md).
