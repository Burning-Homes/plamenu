# Administration and moderation

The administration interface requires a role with the relevant permission.
Assign each moderator their own account and role.

## Before opening registration

- Write clear server rules and a concise description of the community.
- Choose closed, invite, approval, or open registration.
- Test SMTP confirmation and password reset from an external mailbox.
- Set upload, retention, and automated-deletion limits that fit available
  storage and moderation capacity.
- Decide federation policy and document when the server limits or blocks a
  remote domain.
- Create moderation roles with only the permissions each person needs.
- Confirm members can find a private contact path for urgent reports.

## Daily work

### Command-line administration

One CLI command can run alongside one `plamenu serve` process against the same
database. Use the same binary version and configuration as the running server;
serialize scripts that invoke several CLI commands. A second concurrent CLI
command fails immediately, including diagnostic commands that initialize the
database before reading it. Commands with specific stopped-write requirements,
such as a coordinated backup, still require that maintenance window.

During an upgrade, finish active CLI commands before replacing the server and
use the new binary for subsequent administration. See
[writer-lock troubleshooting](troubleshooting.md#writer-lock-errors) if a
command is refused or the process exits after losing its database session.

### Console administration

The console groups work into accounts, reports, appeals, groups, instance and
domain policy, federation diagnostics, trends, invites, rules, announcements,
custom emoji, webhooks, roles, terms, settings, and an audit log.

Record a useful reason for moderation actions. Review reports and appeals as
distinct stages, and verify whether an object is local, cached from a remote
server, or already unavailable before acting. Remote reports and actions cross
organizational boundaries and may reveal information to another operator.

## Personal custom emoji moderation

The **Settings → Custom emoji** group sets the per-user limit (`0` means
unlimited) and the largest accepted emoji file. Open an individual local
account's collection from that account's moderation page; the custom emoji
console provides seven-day trending views for federated origins, all locally
hosted emoji, or personal-only emoji.
`Manage custom emoji` holders can disable, rename, recategorize, remove, or
promote a personal emoji. Promotion makes one copy instance-wide and retires
equivalent personal copies from pickers and quotas while preserving historical
references. A conflicting instance shortcode must be changed explicitly; an
existing emoji is never overwritten.

The `Upload and borrow personal custom emoji` role permission is granted to
the built-in User role by default and can be revoked independently. The shield
menu action is named **Import emojis server-wide**; member borrowing remains in
the ordinary `…` menu as **Borrow custom emojis**. Trend accounting covers the trailing seven days and
ranks distinct users before total public, unlisted, and local post/reaction
uses; private and direct activity is excluded.

## Webxdc sessions and storage

**Settings → Webxdc apps** controls package uploads and federated package
fetches. Defaults are 256 MiB per package, 512 MiB after expansion, and
256 MiB per individual file. Archive path, entry-count and compression-ratio
checks still apply.

The `Create Webxdc sessions` member permission is enabled by default. Revoke it
in **Roles** to prevent new hosted sessions; joining existing sessions remains
available. It grants no staff access. `Manage all Webxdc sessions` separately
grants the **Administration → Webxdc** console (enabled for Admin and Owner).

Defaults allow 1024 MiB per session, 1024 MiB across sessions created by one
local account, and 10240 MiB across the server, including remote caches.
An identical package shares one ZIP and one expanded copy across all sessions.
Each account's quota counts its distinct packages once, and the server quota
counts each stored package once. Durable updates count separately per session;
ephemeral packets are not stored. These are payload budgets, excluding database
row/index overhead and browser-local storage. The session budget must cover the
package and expanded limits combined; the account budget must cover one session.

The console lists hosted sessions and remote caches, including ended sessions,
with package size, reference count and session-data size. Session details show
how much deletion would release. End or delete a hosted session through the
normal federated lifecycle, or remove a remote cache and leave for all local
participants. Actions are audited. Shared files are released only after the
last referencing session is removed. Existing duplicates are consolidated on
upgrade. Members can see their quota usage on the Apps page.

Changes take effect without a restart. Existing apps remain available when
limits are lowered; new uploads, fetches and durable updates must fit the new
limits. The upload form shows the current package limit and reports rejected
uploads without discarding the session name and description.

## Federation policy

Account-level mute/block, domain moderation, and server-wide federation policy
solve different problems. Prefer the narrowest effective control. Before a
domain-wide action, assess effects on existing follows, cached content, pending
deliveries, and members who rely on contacts there.

Use federation debug and the read-only `plamenu federation` commands to gather
evidence. A remote timeout does not by itself prove a block; DNS, TLS, signature
verification, authorized fetch, overloaded queues, and remote policy can
produce similar symptoms.

## High-risk actions

Account deletion, group deletion, key revocation, key-store contraction, and
server self-destruct are intentionally consequential. Read their command help
and specialized runbooks, take a current backup where recovery is possible,
and verify the target before running the command. Self-destruct is irreversible from
the federation's perspective even though local data remains while notices drain.
