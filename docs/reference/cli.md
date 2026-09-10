# Command-line reference

This page is generated from the running program's Clap help tree. Do not edit
it by hand; run `./dev docs-generate` after changing a command or flag.
All commands except `config generate` read the global `--config` TOML file.

## `plamenu`

```text
$ plamenu --help
A fast, small ActivityPub server

Usage: plamenu [OPTIONS] <COMMAND>

Commands:
  config         Create Plamenu configuration
  serve          Run the HTTP server (includes the delivery worker)
  account        Manage local accounts
  group          Manage local groups
  post           Post a status as a local account (enqueued for follower delivery)
  follow         Follow a remote account (`user@domain`) as a local account
  emoji          Manage this instance's custom emoji
  role           Inspect moderation roles
  rule           Manage this instance's published rules (server policies)
  announcement   Manage server announcements (shown to logged-in users)
  media          Maintenance tasks for stored media
  federation     Read-only federation diagnostics (fetch objects, resolve handles, inspect the delivery queue and the reachability breaker)
  self-destruct  Erase the server from the federation (Mastodon's `tootctl self-destruct`): broadcast account deletion notices to every known server, and serve 410 Gone while they go out. Irreversible; always asks for confirmation. Re-run to see the wind-down's progress
  help           Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
  -V, --version          Print version
```

### `plamenu config`

```text
$ plamenu config --help
Create Plamenu configuration

Usage: plamenu config [OPTIONS] <COMMAND>

Commands:
  generate  Write a documented starter TOML file to the --config path
  help      Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu config generate`

```text
$ plamenu config generate --help
Write a documented starter TOML file to the --config path

Usage: plamenu config generate [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --force            Replace an existing file instead of preserving it
  -h, --help             Print help
```

### `plamenu serve`

```text
$ plamenu serve --help
Run the HTTP server (includes the delivery worker)

Usage: plamenu serve [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu account`

```text
$ plamenu account --help
Manage local accounts

Usage: plamenu account [OPTIONS] <COMMAND>

Commands:
  add       Create a local account, optionally with login credentials
  passwd    Set (or replace) the login credentials of an existing account
  set-role  Grant a moderation role to an account (by role name, e.g. `Owner`), or clear it with `--clear`. Use this to bootstrap the first administrator
  rename    Change the human/discovery handle of an immutable-ID account. The old profile URL remains reserved as a redirect; the AP actor ID is stable
  alias     Manage an account's aliases (`alsoKnownAs`) — declare an alias so another account is allowed to migrate its followers here
  migrate   Migrate this account to another (remote) account, re-pointing local followers and telling remote followers via `Move`. The destination must already list this account in its aliases
  help      Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu account add`

```text
$ plamenu account add --help
Create a local account, optionally with login credentials

Usage: plamenu account add [OPTIONS] <USERNAME>

Arguments:
  <USERNAME>

Options:
      --config <CONFIG>              TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --display-name <DISPLAY_NAME>  [default: ""]
      --email <EMAIL>                Optional e-mail: an alternate login identifier that also enables password reset (requires --password)
      --password <PASSWORD>          Password for signing in (via username, or --email when given)
  -h, --help                         Print help
```

#### `plamenu account passwd`

```text
$ plamenu account passwd --help
Set (or replace) the login credentials of an existing account

Usage: plamenu account passwd [OPTIONS] --password <PASSWORD> <USERNAME>

Arguments:
  <USERNAME>

Options:
      --config <CONFIG>      TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --email <EMAIL>        Optional e-mail; omitting it keeps any address already stored
      --password <PASSWORD>
  -h, --help                 Print help
```

#### `plamenu account set-role`

```text
$ plamenu account set-role --help
Grant a moderation role to an account (by role name, e.g. `Owner`), or clear it with `--clear`. Use this to bootstrap the first administrator

Usage: plamenu account set-role [OPTIONS] <USERNAME> [ROLE]

Arguments:
  <USERNAME>
  [ROLE]      Role name (case-insensitive); omit with `--clear`

Options:
      --clear            Remove any assigned role instead of setting one
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu account rename`

```text
$ plamenu account rename --help
Change the human/discovery handle of an immutable-ID account. The old profile URL remains reserved as a redirect; the AP actor ID is stable

Usage: plamenu account rename [OPTIONS] <USERNAME> <NEW_USERNAME>

Arguments:
  <USERNAME>
  <NEW_USERNAME>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu account alias`

```text
$ plamenu account alias --help
Manage an account's aliases (`alsoKnownAs`) — declare an alias so another account is allowed to migrate its followers here

Usage: plamenu account alias [OPTIONS] <COMMAND>

Commands:
  add     Declare an alias (a `user@domain` acct or actor URI) on an account
  list    List an account's declared aliases
  remove  Remove a declared alias (by its stored URI)
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu account alias add`

```text
$ plamenu account alias add --help
Declare an alias (a `user@domain` acct or actor URI) on an account

Usage: plamenu account alias add [OPTIONS] <USERNAME> <ALIAS>

Arguments:
  <USERNAME>
  <ALIAS>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu account alias list`

```text
$ plamenu account alias list --help
List an account's declared aliases

Usage: plamenu account alias list [OPTIONS] <USERNAME>

Arguments:
  <USERNAME>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu account alias remove`

```text
$ plamenu account alias remove --help
Remove a declared alias (by its stored URI)

Usage: plamenu account alias remove [OPTIONS] <USERNAME> <ALIAS>

Arguments:
  <USERNAME>
  <ALIAS>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu account migrate`

```text
$ plamenu account migrate --help
Migrate this account to another (remote) account, re-pointing local followers and telling remote followers via `Move`. The destination must already list this account in its aliases

Usage: plamenu account migrate [OPTIONS] <USERNAME> <TARGET>

Arguments:
  <USERNAME>
  <TARGET>    Destination `user@domain` acct or actor URI

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu group`

```text
$ plamenu group --help
Manage local groups

Usage: plamenu group [OPTIONS] <COMMAND>

Commands:
  add       Create a local group owned by an existing local account
  list      List local groups
  lock      Lock a thread in a group — no new comments (moderator action)
  unlock    Reopen a locked thread
  remove    Remove a post or comment from a group (moderator removal; the status itself survives)
  ban       Ban an account from a group (outcast). `target` is a local username or a known `user@domain` handle
  unban     Lift a group ban
  rename    Rename a group (change its display name), preserving every other setting. Federates the profile Update
  transfer  Transfer ownership of a group to another local member. The previous owner is demoted to moderator
  delete    Delete a group: tombstones the actor (`410 Gone`), federates `Delete(Group)` (Lemmy) plus `Delete(Actor)` (Mastodon), and purges its content. Irreversible
  help      Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group add`

```text
$ plamenu group add --help
Create a local group owned by an existing local account

Usage: plamenu group add [OPTIONS] --owner <OWNER> <NAME>

Arguments:
  <NAME>  The group's name — its `preferredUsername`, sharing the local account namespace (`!name@domain` / `@name@domain`)

Options:
      --config <CONFIG>              TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --owner <OWNER>                Local username of the owner
      --display-name <DISPLAY_NAME>  [default: ""]
      --approval                     Hold join requests for moderator approval instead of auto-accepting followers
  -h, --help                         Print help
```

#### `plamenu group list`

```text
$ plamenu group list --help
List local groups

Usage: plamenu group list [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group lock`

```text
$ plamenu group lock --help
Lock a thread in a group — no new comments (moderator action)

Usage: plamenu group lock [OPTIONS] <GROUP> <STATUS_ID>

Arguments:
  <GROUP>      The group's name
  <STATUS_ID>  The thread root (or any post in it) as a local status id

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group unlock`

```text
$ plamenu group unlock --help
Reopen a locked thread

Usage: plamenu group unlock [OPTIONS] <GROUP> <STATUS_ID>

Arguments:
  <GROUP>
  <STATUS_ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group remove`

```text
$ plamenu group remove --help
Remove a post or comment from a group (moderator removal; the status itself survives)

Usage: plamenu group remove [OPTIONS] <GROUP> <STATUS_ID>

Arguments:
  <GROUP>
  <STATUS_ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --reason <REASON>  [default: "Removed by moderator"]
  -h, --help             Print help
```

#### `plamenu group ban`

```text
$ plamenu group ban --help
Ban an account from a group (outcast). `target` is a local username or a known `user@domain` handle

Usage: plamenu group ban [OPTIONS] <GROUP> <TARGET>

Arguments:
  <GROUP>
  <TARGET>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group unban`

```text
$ plamenu group unban --help
Lift a group ban

Usage: plamenu group unban [OPTIONS] <GROUP> <TARGET>

Arguments:
  <GROUP>
  <TARGET>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu group rename`

```text
$ plamenu group rename --help
Rename a group (change its display name), preserving every other setting. Federates the profile Update

Usage: plamenu group rename [OPTIONS] --display-name <DISPLAY_NAME> <GROUP>

Arguments:
  <GROUP>  The group's name

Options:
      --config <CONFIG>              TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --display-name <DISPLAY_NAME>
  -h, --help                         Print help
```

#### `plamenu group transfer`

```text
$ plamenu group transfer --help
Transfer ownership of a group to another local member. The previous owner is demoted to moderator

Usage: plamenu group transfer [OPTIONS] --to <TO> <GROUP>

Arguments:
  <GROUP>  The group's name

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --to <TO>          Local username (or `@user`) of the new owner — must be a member
  -h, --help             Print help
```

#### `plamenu group delete`

```text
$ plamenu group delete --help
Delete a group: tombstones the actor (`410 Gone`), federates `Delete(Group)` (Lemmy) plus `Delete(Actor)` (Mastodon), and purges its content. Irreversible

Usage: plamenu group delete [OPTIONS] <GROUP>

Arguments:
  <GROUP>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu post`

```text
$ plamenu post --help
Post a status as a local account (enqueued for follower delivery)

Usage: plamenu post [OPTIONS] <USERNAME> <TEXT>

Arguments:
  <USERNAME>
  <TEXT>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu follow`

```text
$ plamenu follow --help
Follow a remote account (`user@domain`) as a local account

Usage: plamenu follow [OPTIONS] <USERNAME> <TARGET>

Arguments:
  <USERNAME>
  <TARGET>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu emoji`

```text
$ plamenu emoji --help
Manage this instance's custom emoji

Usage: plamenu emoji [OPTIONS] <COMMAND>

Commands:
  add     Add a local custom emoji from an image file (PNG, GIF or WebP, at most 256 KB)
  list    List local custom emoji
  remove  Remove a local custom emoji by shortcode
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu emoji add`

```text
$ plamenu emoji add --help
Add a local custom emoji from an image file (PNG, GIF or WebP, at most 256 KB)

Usage: plamenu emoji add [OPTIONS] <SHORTCODE> <FILE>

Arguments:
  <SHORTCODE>  The `:shortcode:` (without colons): 2-128 letters, digits or `_`
  <FILE>       Path to the image file

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu emoji list`

```text
$ plamenu emoji list --help
List local custom emoji

Usage: plamenu emoji list [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu emoji remove`

```text
$ plamenu emoji remove --help
Remove a local custom emoji by shortcode

Usage: plamenu emoji remove [OPTIONS] <SHORTCODE>

Arguments:
  <SHORTCODE>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu role`

```text
$ plamenu role --help
Inspect moderation roles

Usage: plamenu role [OPTIONS] <COMMAND>

Commands:
  list  List the available roles and their permission bitmasks
  help  Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu role list`

```text
$ plamenu role list --help
List the available roles and their permission bitmasks

Usage: plamenu role list [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu rule`

```text
$ plamenu rule --help
Manage this instance's published rules (server policies)

Usage: plamenu rule [OPTIONS] <COMMAND>

Commands:
  list    List the published rules in display order
  add     Add a rule. New rules are appended after the existing ones
  edit    Edit an existing rule's text and/or hint by id
  remove  Remove a rule by id (soft-deleted; reports that cite it still resolve)
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu rule list`

```text
$ plamenu rule list --help
List the published rules in display order

Usage: plamenu rule list [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu rule add`

```text
$ plamenu rule add --help
Add a rule. New rules are appended after the existing ones

Usage: plamenu rule add [OPTIONS] <TEXT>

Arguments:
  <TEXT>  The rule text shown to users (max 300 characters)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --hint <HINT>      Optional longer explanation shown beneath the rule [default: ""]
  -h, --help             Print help
```

#### `plamenu rule edit`

```text
$ plamenu rule edit --help
Edit an existing rule's text and/or hint by id

Usage: plamenu rule edit [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --config <CONFIG>      TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --text <TEXT>
      --hint <HINT>
      --priority <PRIORITY>  Reposition the rule in the ordered list
  -h, --help                 Print help
```

#### `plamenu rule remove`

```text
$ plamenu rule remove --help
Remove a rule by id (soft-deleted; reports that cite it still resolve)

Usage: plamenu rule remove [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu announcement`

```text
$ plamenu announcement --help
Manage server announcements (shown to logged-in users)

Usage: plamenu announcement [OPTIONS] <COMMAND>

Commands:
  list       List all announcements (published and not), newest first
  add        Add an announcement. It publishes immediately unless `--scheduled-at` is set to a future RFC 3339 timestamp
  publish    Publish an announcement by id
  unpublish  Unpublish an announcement by id
  remove     Remove an announcement by id (also clears its reactions and dismissals)
  help       Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu announcement list`

```text
$ plamenu announcement list --help
List all announcements (published and not), newest first

Usage: plamenu announcement list [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu announcement add`

```text
$ plamenu announcement add --help
Add an announcement. It publishes immediately unless `--scheduled-at` is set to a future RFC 3339 timestamp

Usage: plamenu announcement add [OPTIONS] <TEXT>

Arguments:
  <TEXT>  The announcement text (linkified; supports @mentions and #hashtags)

Options:
      --config <CONFIG>              TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --scheduled-at <SCHEDULED_AT>  Hold the announcement unpublished until this time (RFC 3339)
  -h, --help                         Print help
```

#### `plamenu announcement publish`

```text
$ plamenu announcement publish --help
Publish an announcement by id

Usage: plamenu announcement publish [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu announcement unpublish`

```text
$ plamenu announcement unpublish --help
Unpublish an announcement by id

Usage: plamenu announcement unpublish [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu announcement remove`

```text
$ plamenu announcement remove --help
Remove an announcement by id (also clears its reactions and dismissals)

Usage: plamenu announcement remove [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu media`

```text
$ plamenu media --help
Maintenance tasks for stored media

Usage: plamenu media [OPTIONS] <COMMAND>

Commands:
  backfill-sizes  Fill in missing stored byte sizes for media, avatars, headers and emoji (backing the admin storage metrics). Stats each file on the media store; skips any whose file is missing
  reconcile       Find stored files no database row references — orphans left by deletions that ran before the durable cleanup queue existed — and (with `--delete`) schedule them for removal. Reports counts only by default. Run during low upload activity: a brand-new upload whose row is not yet inserted would otherwise look orphaned
  help            Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu media backfill-sizes`

```text
$ plamenu media backfill-sizes --help
Fill in missing stored byte sizes for media, avatars, headers and emoji (backing the admin storage metrics). Stats each file on the media store; skips any whose file is missing

Usage: plamenu media backfill-sizes [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu media reconcile`

```text
$ plamenu media reconcile --help
Find stored files no database row references — orphans left by deletions that ran before the durable cleanup queue existed — and (with `--delete`) schedule them for removal. Reports counts only by default. Run during low upload activity: a brand-new upload whose row is not yet inserted would otherwise look orphaned

Usage: plamenu media reconcile [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --delete           Actually enqueue the orphans for deletion (default: report only)
  -h, --help             Print help
```

### `plamenu federation`

```text
$ plamenu federation --help
Read-only federation diagnostics (fetch objects, resolve handles, inspect the delivery queue and the reachability breaker)

Usage: plamenu federation [OPTIONS] <COMMAND>

Commands:
  fetch         Fetch a remote `ActivityPub` object and pretty-print it. Signed GET, following a permalink to the canonical `id` (paste a status URL or an actor URL). Nothing is stored
  webfinger     Resolve a `user@domain` handle via `WebFinger` and print every `ActivityPub` actor it advertises. Nothing is stored (unlike a real follow, no remote account row is created)
  queue         Inspect the outbound delivery queue
  reachability  List the hosts the delivery breaker currently considers unreachable, with their failure streak and last error
  keys          Inspect, rotate, revoke, or rewrap normalized federation keys
  help          Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu federation fetch`

```text
$ plamenu federation fetch --help
Fetch a remote `ActivityPub` object and pretty-print it. Signed GET, following a permalink to the canonical `id` (paste a status URL or an actor URL). Nothing is stored

Usage: plamenu federation fetch [OPTIONS] <URL>

Arguments:
  <URL>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu federation webfinger`

```text
$ plamenu federation webfinger --help
Resolve a `user@domain` handle via `WebFinger` and print every `ActivityPub` actor it advertises. Nothing is stored (unlike a real follow, no remote account row is created)

Usage: plamenu federation webfinger [OPTIONS] <ACCT>

Arguments:
  <ACCT>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu federation queue`

```text
$ plamenu federation queue --help
Inspect the outbound delivery queue

Usage: plamenu federation queue [OPTIONS] <COMMAND>

Commands:
  inspect  Summarize the queue: total/due counts, when the next job fires, and the worst per-host backlogs
  help     Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu federation queue inspect`

```text
$ plamenu federation queue inspect --help
Summarize the queue: total/due counts, when the next job fires, and the worst per-host backlogs

Usage: plamenu federation queue inspect [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu federation reachability`

```text
$ plamenu federation reachability --help
List the hosts the delivery breaker currently considers unreachable, with their failure streak and last error

Usage: plamenu federation reachability [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

#### `plamenu federation keys`

```text
$ plamenu federation keys --help
Inspect, rotate, revoke, or rewrap normalized federation keys

Usage: plamenu federation keys [OPTIONS] <COMMAND>

Commands:
  audit            Fail unless all private rows are encrypted, decryptable, and match their public halves
  rewrap           Re-encrypt rows from configured previous secrets onto the primary encryption-secret version. Safe to resume
  contract         Permanently drop the verified-empty legacy plaintext columns after all processes have been upgraded and the rollback window has ended
  rotate-account   Rotate one local account signing algorithm with a bounded overlap
  rotate-instance  Rotate an instance-actor signing algorithm with a bounded overlap
  revoke           Immediately revoke a key URI; it can no longer sign or verify
  retire           Retire a key URI after its overlap/grace use is complete
  help             Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu federation keys audit`

```text
$ plamenu federation keys audit --help
Fail unless all private rows are encrypted, decryptable, and match their public halves

Usage: plamenu federation keys audit [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu federation keys rewrap`

```text
$ plamenu federation keys rewrap --help
Re-encrypt rows from configured previous secrets onto the primary encryption-secret version. Safe to resume

Usage: plamenu federation keys rewrap [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --limit <LIMIT>    Process at most this many stale rows, for controlled rolling rehearsals. Omit to drain every bounded batch
  -h, --help             Print help
```

##### `plamenu federation keys contract`

```text
$ plamenu federation keys contract --help
Permanently drop the verified-empty legacy plaintext columns after all processes have been upgraded and the rollback window has ended

Usage: plamenu federation keys contract [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu federation keys rotate-account`

```text
$ plamenu federation keys rotate-account --help
Rotate one local account signing algorithm with a bounded overlap

Usage: plamenu federation keys rotate-account [OPTIONS] --algorithm <ALGORITHM> <USERNAME>

Arguments:
  <USERNAME>

Options:
      --algorithm <ALGORITHM>
          [possible values: rsa, ed25519]
      --config <CONFIG>
          TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --overlap-hours <OVERLAP_HOURS>
          [default: 168]
      --activation-delay-seconds <ACTIVATION_DELAY_SECONDS>
          Publish the replacement before using it to sign, giving peers time to process the old-key-signed Actor Update [default: 300]
  -h, --help
          Print help
```

##### `plamenu federation keys rotate-instance`

```text
$ plamenu federation keys rotate-instance --help
Rotate an instance-actor signing algorithm with a bounded overlap

Usage: plamenu federation keys rotate-instance [OPTIONS] --algorithm <ALGORITHM>

Options:
      --algorithm <ALGORITHM>
          [possible values: rsa, ed25519]
      --config <CONFIG>
          TOML configuration file used by every command except `config generate` [default: plamenu.toml]
      --overlap-hours <OVERLAP_HOURS>
          [default: 168]
      --activation-delay-seconds <ACTIVATION_DELAY_SECONDS>
          [default: 300]
  -h, --help
          Print help
```

##### `plamenu federation keys revoke`

```text
$ plamenu federation keys revoke --help
Immediately revoke a key URI; it can no longer sign or verify

Usage: plamenu federation keys revoke [OPTIONS] <KEY_URI>

Arguments:
  <KEY_URI>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

##### `plamenu federation keys retire`

```text
$ plamenu federation keys retire --help
Retire a key URI after its overlap/grace use is complete

Usage: plamenu federation keys retire [OPTIONS] <KEY_URI>

Arguments:
  <KEY_URI>

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```

### `plamenu self-destruct`

```text
$ plamenu self-destruct --help
Erase the server from the federation (Mastodon's `tootctl self-destruct`): broadcast account deletion notices to every known server, and serve 410 Gone while they go out. Irreversible; always asks for confirmation. Re-run to see the wind-down's progress

Usage: plamenu self-destruct [OPTIONS]

Options:
      --config <CONFIG>  TOML configuration file used by every command except `config generate` [default: plamenu.toml]
  -h, --help             Print help
```
