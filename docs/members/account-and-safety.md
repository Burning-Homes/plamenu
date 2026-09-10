# Account settings and safety

Open **Settings** to change your profile, preferences, languages, privacy,
notifications, filters, relationships, security, and data-transfer settings.
Profile changes may take time to reach remote servers.

## Privacy

**Privacy & reach** controls follower approval, default post visibility,
profile discovery, search-engine indexing, and direct remote media loading.
Requiring approval applies to new followers; review existing followers
separately. Direct remote-media fallback is enabled by default when the server
cannot cache a file. This exposes your browser's IP address to the media host
when the browser loads from it; turn **Allow direct remote media** off to keep
media requests on your instance.

## Live notifications

Live notifications are off by default. Enabling them in the built-in client
can disrupt navigation context and scroll position; see [Known issues](../KNOWN_ISSUES.md).

## Filters, mutes, blocks, and reports

Filters hide or warn on matching content in selected contexts. Muting removes
an account's activity from your view. Blocking restricts interaction with that
account. Use **Report** in a post or profile menu to send a case to your server's
moderators. The form may offer to forward it to the remote server as well.

## Sign-in security

**Security** offers TOTP, WebAuthn security keys, active sessions, and sign-in
history. Store recovery codes somewhere you can access if you lose your usual
factor. Review authorized applications and revoke access you no longer use.
Contact your server's operators if you cannot recover access.

## Import, export, and migration

**Import & export** provides CSV transfers and full account archives. An account
archive is a downloadable snapshot; it cannot restore a whole server.

To move followers, first add the old account as an alias at the destination,
then start migration from the old account. Other servers process the move
asynchronously. Posts and all other account data do not move with followers.

## Deletion

Export anything you need before deleting an account. Plamenu queues local
cleanup and remote deletion notices. Remote servers and independent archives
may retain copies.

## Identity proofs (experimental)

**Settings → Identity proofs** lets you publish an FEP-c390 statement linking
your account to an Ed25519 key you own. Verified keys appear on profiles, with
a link to the signed statements. A proof establishes control of a key; it does
not establish a person's legal identity or authorize account migration.

Copy the actor ID shown in settings. From the source checkout, create a key
and sign a statement locally with [uv](https://docs.astral.sh/uv/):

```sh
uv run scripts/sign-identity-proof.py --generate --key identity.pem \
  --actor 'https://your-server.example/ap/accounts/your-account-id' > statement.json
```

Use your actual actor ID. Paste `statement.json` into settings and publish it.
Keep `identity.pem` private and backed up. To sign for another account with the
same key, run the command with its actor ID and omit `--generate`.
**Remove proof** withdraws the statement and notifies connected peers.
