# Native media storage and replication

The normal installation still uses one `media_dir` and the existing bounded
native workers. No runner, object-storage service or synchronization daemon is
required. `[native_video].jobs` selects one to four native processing slots;
the default remains two. These are not separate database-writing servers.

## Optional paired disks

An operator who already has a second mounted disk can opt into built-in
replication of durable flat media keys and asynchronous live checkpoints:

```toml
[media_mirror]
directory = "/srv/plamenu-replica"
bandwidth_mib_per_second = 32
min_free_gib = 1
```

The second directory must already exist, be empty on first use, and belong to
the Plamenu service account. The roots must be separate, non-nested directories.
Use independent failure domains; two directories on the same disk are not
hardware redundancy. Pairing markers are retained on both roots. Missing or
mismatched markers stop startup and refuse writes/deletion acknowledgements;
Plamenu does not silently recreate a missing mount as an empty replica.
Do not remove or overwrite these markers to bypass an error.

On server startup, existing flat primary keys are checked and seeded serially
before opening the listener. Equal size is not treated as equal content. A
failed seed can be restarted safely. Allow time for the initial copy and
subsequent verification; ordinary account-management CLI commands do not
perform this full scan. The primary is authoritative during seeding. This is
not automatic repair of a corrupt primary from a known-good backup.

Successful writes acknowledge synced primary and replica data. Replica files
are staged privately and atomically renamed. Resumable chunks acknowledge both
prefixes before the database advances their offset; retries truncate uncommitted
tails. Missing or short prefixes can be recovered from the surviving copy.
Copies use a 64 KiB buffer and one serialized bandwidth budget, including
resumable appends. The free-space floor is checked on both disks.

Reads prefer the primary and fall back to the replica without loading a whole
movie into memory. Public/private publication, moderation and cleanup checks
still precede either disk; a replica never grants permission. Deletion is only
acknowledged after both roots remove the key and sync their directories.
Failure retains cleanup debt and revocation instead of refunding quota early.
Storage accounting charges one logical object, but actual durable disk use is
approximately twice that charge, plus temporary copies. Budget both disks.

Cancelled copies remove their owned staging file. Startup also removes only
regular files matching the exact generated staging format whose last write is
over 48 hours old. Recent copies, symlinks and unrelated operator files are
preserved. An unacknowledged failed write can leave an orphan primary key;
ordinary acknowledged media reconciliation handles it.

## Active live checkpoints

The same option checkpoints closed native HLS segments and complete fragmented
MP4 recording prefixes. There is one background worker per admitted publisher,
one pending operation and a two-second tick. A busy durable-media writer skips
a tick; no snapshot queue or second encode is introduced. Copies share the
existing serialized bandwidth budget, 64 KiB buffers, paired mount checks and
free-space floor. Each checkpoint has a twenty-second deadline.

Segments are synced before atomically publishing their playlist. The current
and previous committed windows are retained for in-flight readers; failed-copy
leftovers are bounded on subsequent attempts. Primary loss of a committed
segment can fall back to its replica through the usual canonical HTTP routes,
including integrity manifests and the shared progressive remux. Replica HLS
playlists expire thirty seconds after publication, and an already stale primary
playlist cannot renew a replica. Authority and exact session UUID fences still
precede either copy. Off-air, deleted or moderated media stays inaccessible.

The recording checkpoint scans box headers, then copies only the newly closed
`moof`/`mdat` pairs, not an incomplete growing tail. Data is synced before its
private committed-offset marker. A crash before marker commit leaves a tail
that is ignored on recovery and truncated on the next append. At startup, a
recoverable database session with missing primary recording is restored from
exactly the committed replica prefix before normal reconnect/expiry/replay
processing. Existing primary recordings are never overwritten by recovery.

Successful replay/superseded-session cleanup removes both generation directories.
Failed replay processing retains its recordings. A missing replica defers its
scratch cleanup to startup reconciliation; only abandoned canonical numeric/
UUID directories are removed. Unrelated files/symlinks are preserved. Owned
checkpoint temporaries older than 48 hours are removed during checkpoint work.

These are **asynchronous checkpoints, not synchronous live durability or
seamless publisher/disk failover**. Under contention or failure the last
recording prefix can lag without a guaranteed recovery-point interval. Log
transitions report `live checkpoint degraded` / `live checkpoint recovered`;
no successful durable-media acknowledgement implies a fresh live checkpoint.
If the publisher's primary disk fails, its encoder can still stop. Restore the
paired disk before restart; startup never invents missing mount markers.
Finite no-JS rewind clips can now be regenerated from a fresh, owned replica
when the primary recording is absent. The builder pins only closed segments
in the selected five-minute portion with private hard links, remuxes once and
removes its pins afterward. It reserves 1 GiB from the shared native processing
pool before pinning/remuxing, has finite admission/build deadlines and rechecks
the exact current publication/session before responding. Both source and clip
have byte ceilings. These regenerable clips are not another durable recording;
this does not provide seamless publisher disk failover or guaranteed RPO.

## Remote range-cache retirement

Remote HLS and progressive range-cache writes have a separate admission pool:
eight active writes and 512 MiB of conservative scratch/copy reservations,
with a five-second admission deadline. Failed admission returns a retriable
503 rather than starting an unbudgeted write. A cancelled viewer does not
release the single-flight/storage fence while its owned final write continues.

Every new key contains fresh entropy; a refetch never reuses a retired key.
A private, byte-charged pre-write cleanup journal precedes filesystem writes.
Its row lock prevents cleanup from acknowledging a live writer. Only a
successful cache-row commit releases that journal. Failed writes become due
for cleanup; crash leftovers become due after one hour. They are never public
merely because a restored filesystem contains their bytes.

Migration 0129 atomically transfers a retired range row's known byte charge
to cleanup debt, including cascaded deletion. Expiry, replacement and deletion
do not refund that charge until the cleanup worker acknowledges physical
removal from both configured roots. Failed copies/deletions retain private
keys and charges. Logical byte accounting still charges one copy, not two.
The live in-memory cache is separately bounded by 256 MiB, 4,096 entries,
8,192-byte keys and a five-minute TTL, including ordering/index metadata.

The ordinary hourly retention worker remains automatic. An operator can run
the same configured pass explicitly with `plamenu --config plamenu.toml media
expire-caches`; inspect its warnings and pending-cleanup count. The command
does not claim failed physical removals completed. No new daemon is required.
Include cleanup journals and both media roots in a consistent backup.

## Boundaries and recovery

The mirrored surface includes completed native MP4/HLS/posters/storyboards,
retained sources, receiving upload prefixes, completed live replays and flat
remote media-cache keys. Active HLS/recording checkpoints are described above;
other derived scratch remains on `media_dir`.
This option is not a second serving node, remote-worker protocol or PeerTube
redundancy-import service. Those wider operations gates remain explicitly open.

Replication is not a backup: authorized deletions reach both disks, and
primary corruption can propagate during seeding. Follow
[Backup and restore](operators/backup-and-restore.md): quiesce writers and keep
the database, entire primary media tree (including active recoverable recordings),
configuration/key escrow and pairing markers consistent. Preserve the entire
replica as well when using this option. Never restore a database snapshot with
arbitrarily mixed media generations. A missing disk requires restoration of its
paired directory; do not point the running service at an unowned replacement.

Disabling `[media_mirror]` returns to the normal store but does not erase the
replica. First verify that every still-referenced durable key exists on the
primary, especially after failover. Retire the replica explicitly according to
your retention/backup policy; Plamenu never erases a configured storage root.
