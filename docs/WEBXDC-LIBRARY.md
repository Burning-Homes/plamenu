# Webxdc app libraries and catalogs

Plamenu separates reusable apps from running Webxdc sessions. A library app is
a mutable identity with one current version; each version points to an immutable
SHA-256 multihash package in the same storage used by FEP-752d sessions. Starting
a session pins that exact digest. Updating or removing a library entry never
changes a running session.

## Libraries and moderation

- A member with the `create_webxdc` role permission may keep a personal library,
  upload new versions, remove entries, and choose a personal or instance app when
  creating a session. Direct one-off `.xdc` uploads remain available.
- A moderator with `manage_webxdc` may upload instance apps, promote a personal
  app, add a reviewed version, hide an app, make it available only to local
  members, publish it in the public catalog, or remove it.
- Promotion adds an instance-library reference rather than moving the member's
  entry. A later personal version is shown to moderators as awaiting review and
  is not promoted automatically.
- `hidden` entries remain available to moderators but cannot start new member
  sessions. `instance` entries are available to signed-in local members.
  `public` entries are also exported from `/webxdc/catalog.json`.

The default personal limit is 50 app identities per member. Operators can set it
from the Webxdc settings panel; zero disables personal saving without disabling
one-off session uploads. Package bytes count against the existing per-account
and whole-instance Webxdc storage quotas. Identical package digests count once.
Durable session updates continue to count only toward their session and creator.

Removing an app deletes its versions, not its sessions. Package ZIP and expanded
files are garbage-collected only after the last session and library version have
both released the digest.

## Package metadata

Every upload and import goes through the same ZIP path and resource limits. The
root `manifest.toml` supplies `name` and optional `source_code_url`; without a
name, Plamenu uses the `.xdc` filename. Root `icon.png` and `icon.jpg` are
recognized in that order, with the normal Plamenu app icon as a fallback. The
de-facto `tag_name` manifest field used by current app catalogs is retained as a
version label, but is not treated as a Webxdc protocol requirement.

Malformed or excessively large manifests are rejected. Source links must be
HTTP(S), metadata is length-bounded, and icons remain package assets rather than
being copied into the social-site media store. See the upstream
[Webxdc package format](https://webxdc.org/docs/spec/format.html).

## External catalogs

Plamenu installs the `Webxdc Apps` source at
`https://apps.testrun.org/xdcget-lock.json` by default. A moderator may remove
it or add other sources. Plamenu currently provides an `xdcget-v1` adapter for
the JSON shape consumed by webxdc.org. Refresh is manual and bounded to 2,000
entries. It caches advisory metadata only; browsing a feed does not download or
execute packages.

Signed-in members may browse sources that a moderator has configured and
refreshed, then validate and save an entry to their personal library. Moderators
can instead import an entry into the instance review queue. Import is a separate
action. The package request uses the federation client's
HTTPS/hidden-service policy, DNS-rebinding-resistant SSRF checks, per-hop
redirect validation, timeouts, failure budgets, global admission limit, and the
operator's package-size ceiling. Accepted response types are the two Webxdc
types, ZIP, and generic binary downloads. The downloaded bytes are then hashed,
expanded, and validated locally. Canonical manifest metadata and the resulting
digest replace conflicting feed claims. Imported apps begin `hidden` for review.
Source and bundle URLs remain attached as provenance. Instance imports begin
`hidden`, while personal imports remain private; moderator mutations are written
to the audit log.

Refresh and import stay manual by design: catalogs are executable-software
discovery, not a background update channel. A changed catalog bundle can be
revalidated from the moderation page. No package is silently installed and no
running session is upgraded.

## Public catalog and federation

`GET /webxdc/catalog.json` emits the current version of explicitly `public`
instance apps in the widely deployed xdcget array shape, with additive
`digest_multibase` and `provenance` fields. Version-scoped bundle and icon URLs
are immutable and long-cacheable. Hiding or unpublishing an app removes it from
the catalog and prevents new library sessions; already pinned sessions continue
to work.

An ActivityPub representation was considered alongside the existing
[FEP-752d session model](fep/fep-752d.md). Sessions are actors because they own
membership, an inbox, an outbox, and ordered updates. App identities do none of
those things, so representing them as session actors would conflate executable
artifacts with live collaboration state. ActivityStreams `Collection`,
`Document`, `attributedTo`, `url`, `icon`, and `updated` can describe much of a
catalog, while `digestMultibase` and the Webxdc media types can be reused.
However, there is not yet an interoperable activity vocabulary for app version
lineage, moderation state, or update announcements. Plamenu therefore publishes
the adapter JSON and stable integrity-bearing resources now, without inventing
federation activities peers would not understand. A future FEP can map the same
app/version records without a storage or identity migration.
