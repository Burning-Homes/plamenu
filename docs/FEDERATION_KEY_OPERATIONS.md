# Manage federation signing keys

Plamenu stores local ActivityPub signing keys encrypted in PostgreSQL. The
`encryption_secret` in the server configuration unlocks them. Keep that secret
in a protected configuration backup separate from the database.

The commands below use the Debian service account and configuration path.
For a container, run the same CLI commands as the application user with its
configuration mounted. Use the same binary version as the running server.

## Check the keys

```sh
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml federation keys audit
```

The audit checks that private keys are encrypted, can be decrypted with the
configured keyring, and match their public keys. If it fails, check the secret
versions and restore the matching configuration before replacing any keys.
Do not include private keys, ciphertext, or encryption secrets in bug reports.

## Rotate the encryption secret

**Current limitation:** `federation keys rewrap` covers federation keys only.
TOTP secrets use the current `encryption_secret`; they do not read
`encryption_previous_secrets`. Changing the primary secret can therefore break
existing TOTP sign-in even when the federation audit passes.

Do not perform this procedure on a server with stored TOTP secrets until those
secrets have a supported migration/re-enrollment plan. Check for both completed
and pending enrollments with a read-only query against the correct database:

```sql
SELECT count(*) AS stored_totp_secrets FROM users WHERE otp_secret IS NOT NULL;
```

A nonzero result requires resolving TOTP migration first. Keep the original
secret available for recovery; do not manually erase enrollment data. A passing
federation audit says nothing about TOTP. Test a second-factor sign-in as part
of any later secret-rotation rehearsal.

For an installation with no stored TOTP secrets, the following changes
federation-key encryption while keeping the published signing keys. Prevent new
TOTP enrollment during the change and generate a new secret:

```sh
openssl rand -hex 32
```

In the server configuration, give it a new positive version and retain the
old secret for decryption:

```toml
encryption_secret = "replace-with-the-new-secret"
encryption_secret_version = 2
encryption_previous_secrets = ["1:replace-with-the-old-secret"]
```

Restart Plamenu with that configuration, then re-encrypt existing keys:

```sh
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml federation keys rewrap
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml federation keys audit
```

Rewrapping works in batches and can resume after interruption. `rewrap --limit 100`
processes a limited batch. Once rewrapping and the audit pass, the old
secret can be removed from the live configuration. Retain it in protected
backup storage for as long as any retained database backup needs it. Never
assign different secrets the same version number.

## Rotate the published signing keys

Rotate an account key or the instance key, selecting RSA or Ed25519:

```sh
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation keys rotate-account alice --algorithm rsa
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation keys rotate-account alice --algorithm ed25519
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation keys rotate-instance --algorithm rsa
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation keys rotate-instance --algorithm ed25519
```

A new key is published immediately and starts signing after five minutes by
default. An actor update signed with the old key announces the replacement.
The old key remains available for a seven-day overlap. Use `--overlap-hours`
and `--activation-delay-seconds` to adjust those windows; the activation delay
must be positive so peers have time to learn the new key.

To end use of an old key explicitly, supply its exact URI:

```sh
sudo -u plamenu plamenu --config /etc/plamenu/plamenu.toml \
  federation keys retire 'https://social.example.com/actor#old-key'
```

Use `revoke` in place of `retire` for a compromised key. Revoked, retired,
expired, and not-yet-active keys cannot sign or verify new traffic. Their public
records remain available for audit.

## Restore or recover a lost secret

Restore the database and the encryption-secret versions that can decrypt its
keys. An older database backup may need a version no longer used by the live
server. Test both backups together.

Without any matching secret, the encrypted keys cannot be recovered. Recover
the secret from its backup first. Replacing signing keys is a separate recovery
operation that can disrupt federation with peers caching the old keys.
