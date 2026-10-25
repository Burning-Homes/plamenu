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

## Profile, relationships and collections

Add profile metadata such as your website. A website linking back with `rel="me"`
can earn a link-verification checkmark; that verifies the link, not a legal
identity. Use **Relationships** for follows/followers and requests, **Lists**
for selected followed accounts, and **Featured hashtags** for profile links.

**Collections** curates shareable sets of accounts. Inclusion requires the
account's discovery/featuring consent; pending invitations are not accepted
members. The same page shows collections featuring you and lets you leave them.
These account collections are separate from music collections and video playlists.

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

**Import & export** accepts CSV following, blocking, muting, domain-blocking,
bookmark and list data. Choose **Merge** to add to existing data or **Overwrite**
to replace the selected set. Review the import before confirming, then check
**Recent imports** for successes and failed rows; completion can be partial.

A full account archive is a downloadable snapshot, not a server backup or an
implemented historical-post import. Protect it as private data.

To move followers, first add the old account as an alias at the destination,
then start migration from the old account. Other servers process the move
asynchronously. Posts and all other account data do not move with followers.

## Deletion

**Automated deletion** continuously removes old posts under your chosen age,
exceptions and popularity thresholds. Review the policy before enabling it:
deletions are permanent and federate to other servers.

Export anything you need before deleting an account. Plamenu queues local
cleanup and remote deletion notices. Remote servers and independent archives
may retain copies. Your username remains reserved. Stored music, queues,
history and owned channels are also affected; transfer channels deliberately
before retirement, and remember that your original videos are still deleted.

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
