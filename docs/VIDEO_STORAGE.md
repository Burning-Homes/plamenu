# Native media storage and replication

The normal installation still uses one `media_dir` and the existing bounded
native workers. No runner, object-storage service or synchronization daemon is
required. `[native_video].jobs` selects one to four native processing slots;
the default remains two. These are not separate database-writing servers.

## Optional paired disks

Federation copies are a separate optional facility described under
“Optional federation video copies” below; they are not shared-database web
clustering and do not change the native installation baseline.

An operator who already has a second mounted disk can opt into built-in
replication of durable flat media keys and bounded live durability acknowledgements:

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

## Optional federation video copies

The native server can copy an explicitly consented, public/unlisted complete
VOD from an approved HTTPS source. This adds no runner, synchronization daemon
or mandatory service. It is disabled unless the operator enables:

```toml
[video_redundancy]
quota_gib = 2
max_mirror_mib = 512
bandwidth_mib_per_second = 8
retention_hours = 24
```

There is one leased copier, with a finite job deadline, bounded scratch space,
128-KiB playlist limit and at most 4,096 dependencies/64 playlists. Copies are
paced in 64-KiB portions before writes. The graph must remain on its approved
origin; encrypted, unfinished/live or conflicting-basename layouts are refused,
not published as partial redundancy. Progressive compatibility files and the
canonical checksum sidecar are retained alongside adaptive media, so first-party
JS and non-JS playback can use an acknowledged copy. This software rate limit is
not a physical-disk QoS guarantee. Paired disks additionally apply their own
shared write budget; physical disk allocation remains approximately doubled.

The operator API requires `MANAGE_FEDERATION` and the matching broad
`admin:read`/`admin:write` token scope. Use authenticated requests with these
JSON bodies; never place tokens in URLs or command arguments:

| Method/path | Request or response |
| --- | --- |
| `PUT /api/v1/admin/video_mirror_approvals/{domain}` | `{"allow_source":true,"allow_mirror":false}` approves an exact source; reverse the flags to approve an advertising server |
| `POST /api/v1/admin/video_mirrors` | `{"status_id":123,"consent_attestation":""}` requests an already resolved canonical Video |
| `GET /api/v1/admin/video_mirrors` | Up to 100 latest jobs, current charged bytes and private no-store response |
| `GET /api/v1/admin/video_mirrors/{id}` | State, generation, expiry, charge and failure status |
| `DELETE /api/v1/admin/video_mirrors/{id}` | Withdraws serving and atomically queues the signed Undo |

Creators grant or revoke copying using the first-party HTML media sharing
form's mirror-permission checkbox, or the `mirror_allowed` flag in the existing
distribution API. It defaults to false; merely making a video public or enabling
download does not grant permission. The API replaces all distribution flags:
include the desired download/embed flags when updating permission. The signed
Video carries Plamenu's full-IRI permission extension; a live announcement never
advertises finite-copy permission. For foreign creators without that vocabulary,
an operator must provide private explicit creator-consent evidence in
`consent_attestation` (20–2,000 bytes). This is an operator attestation, not
automatically verified consent or an inference from PeerTube's download flag.

No public CacheFile is advertised before every graph asset has a durable store
acknowledgement and current source authority still matches. Signed Create/Undo
and their state transitions use the same database transaction. Source updates,
withdrawal, expiry, suspension or domain-policy changes deny warm reads. Accepted
remote offers remain actor-bound and require explicit mirror-domain approval;
a replayed Create cannot undo an authenticated withdrawal. A copy never grants
access to a private/deleted original. Only the specialized mirror route may
serve its permanently private storage-key namespace.
Incoming offer bookkeeping currently admits at most 256 distinct known URIs
per advertising actor, including retained withdrawal tombstones. Reaching that
bound refuses new offers rather than forgetting withdrawals or growing without
limit; it is not a claim of unlimited foreign redundancy capacity.

Reservations include in-flight writes. Cancelled finalizers retain their owned
input and database fence until the write actually finishes. Retirement retains
charged physical cleanup debt until the normal media worker acknowledges disk
deletion, including both roots on paired storage. Reconciliation recognizes
live mirror assets and pre-write journals rather than deleting them as orphans.

Actual native and PeerTube-source copying, signed advertisement/withdrawal,
fresh database/media-root recovery, wrong-secret refusal and cold narrow
first-party JS/non-JS decoding are verified in the AP09 closure record. Native
copy playback continues with the original source process stopped. The stock
PeerTube player also decodes actual advertised copy bodies under a stalled
primary-body failure, but still needs its bounded origin MP4 initialization
request: its upstream init loader does not use redundancy. Immediate concurrent
range failures can also trigger its file-URL-keyed redundancy map race. Neither
limitation is claimed as universal cold-origin-loss compatibility. Incoming
offers are authenticated control-plane records, not a promise that every
third-party viewer discovers or uses them. M7's defined local operations gates
are verified in the closure record; these bounded results are not arbitrary
capacity/hardware certification or full AP09 ecosystem completion.

## Optional native processing hosts

The installed executable also supports a private, DB-less video worker. It is
optional: normal single-host installations need no configuration or additional
service. The origin remains the only database/ID writer and publisher. A worker
never receives database credentials, federation keys, account identities or
arbitrary source URLs. It receives only a private upload and bounded encoding
instructions, and returns private outputs for independent origin validation.

On an additional processing host, generate a private file and start the same
binary:

```sh
plamenu --config worker.toml config generate --video-worker
plamenu --config worker.toml video-worker
```

The generated file is owner-only and contains a fresh random token. Keep it
private; copy its token only into approved origins' configuration, never URLs
or command arguments. Configure the worker's dedicated `directory`, tools and
`[native_video]` budgets. Plain HTTP is allowed only on literal loopback. Remote
workers require the built-in `[tls]` certificate/key configuration; origins
verify the certificate with their normal system trust store. Certificate files
reload every thirty seconds, retaining the last valid pair on reload failure.
No reverse proxy or processing-runner service is required.

Add approved workers to the origin file (at most sixteen distinct origins):

```toml
[[video_offload]]
url = "https://processor.example/"
token = "copy-the-private-generated-worker-token-here"
```

All worker routes, including health/status/artifacts, require the private token
and refuse public caching. Admission reserves bounded scratch before upload;
one to four jobs and thirty-two connections are allowed, with finite header,
upload-idle and job deadlines. Leases expire after thirty seconds without
renewal; an origin renews every five seconds while its own database claim is
current. A normal cancelled claim sends an orderly bounded DELETE; origin
crash or an aborted future stops renewal. Expiry stops the encoder and removes only
owned scratch. Output readers retain admission until their transfer ends.
Jobs have a four-hour absolute lifetime. Connections without actual transport progress are closed after
fifteen seconds (sampled at fifteen-second intervals), including a reader
that stalls while pinning completed output; active transfers retain the
four-hour absolute bound. Completed/failed jobs never publish
themselves. Processing crash scratch is scavenged only from the exclusively
locked, ownership-marked worker root; foreign files and symlinks are refused.

Targets are tried once each in round-robin order, then the native local lane
is used on unavailability or invalid output. Original sources remain at the
origin throughout. The origin checks lengths/digests, codecs/dimensions,
generated filenames and playlists, poster/storyboard bounds and independently
recomputed HLS range hashes before its existing quota/journal/claim/publication
transaction. Invalid outputs cannot replace an already published film. Health
reports counts/budgets only, not job identifiers or source paths.

Workers need no database backup: their scratch and leases are regenerable.
Keep the origin's database, retained sources and configuration/key escrow in
the normal consistent backup. Rotate tokens by draining work, updating both
private files and restarting the affected processes. This protocol is native
Plamenu processing, not a PeerTube plugin/runner ABI or shared-database cluster.

## Active live checkpoints

On Linux the same encoder now has independent primary/replica HLS and fragmented
MP4 muxers. A primary EIO/ENOSPC disables that muxer, not the healthy replica
or publisher. The server holds an owned replica directory descriptor; filesystem
paths are never interpolated into the tee protocol. A strict persisted direct-
writer marker prevents the legacy copier from truncating that growing recording.
The background worker fsyncs complete recording prefixes and closed segments,
then publishes a separate durable playlist. HTTP still checks the canonical
publication and exact session. The newest **closed segment**, not just a DVR
list's media sequence, selects the surviving copy after repair. The stale
disabled muxer cannot send a viewer backwards. Linux failure and narrow JS/no-JS
plus actual PeerTube decoder receipts are in the M7 record.

When a direct writer is unavailable, the existing fallback checkpoints closed native HLS segments and complete fragmented
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
Failed replay processing or an unreadable recording retains the generation;
an I/O error is never treated as proof that no recording exists. Closed replay
processing can read a finite complete-fragment prefix through an owned file
descriptor, without copying or truncating the source's incomplete tail.
A missing replica defers its
scratch cleanup to startup reconciliation; only abandoned canonical numeric/
UUID directories are removed. Unrelated files/symlinks are preserved. Owned
checkpoint temporaries older than 48 hours are removed during checkpoint work.

These remain **asynchronous durability acknowledgements**, not synchronous live
durability or guaranteed recovery-point intervals. Independent muxers now
provide measured healthy-replica continuity through primary EIO/ENOSPC; this
does not certify arbitrary lost-mount/hung-device failure modes. Under contention or failure the last
recording prefix can lag without a guaranteed recovery-point interval. Log
transitions report `live checkpoint degraded` / `live checkpoint recovered`;
no successful durable-media acknowledgement implies a fresh live checkpoint.
The legacy primary-only fallback can still stop on primary failure. Direct
copy-format publishers and flat/checkpoint copies share one software byte
schedule, with 64-KiB transfer chunks and no more than two seconds of queued
grants. Each copy-format publisher reserves 256 KiB/s for bounded checkpoint
metadata and charges three times its input tag bytes plus 4096 bytes before
feeding the muxer. Browser WebM re-encoding reserves 2 MiB/s per publisher,
paces input to media wall-clock time and bounds video/AAC bitrate, VBV and
threads. Reservations end when the encoder exits, not after replay/drain work.
Insufficient reservation refuses the independent writer and leaves the
explicitly degraded primary/checkpoint fallback; configure enough bandwidth
for the intended publisher count. This is software admission/pacing, **not
physical disk QoS**: encoded/VBV bursts and already admitted grants can overlap
briefly. It adds no mandatory service or installation dependency.

Real owned replica EIO, both-root ENOSPC/EIO and fresh DB/storage recovery
receipts are recorded in the M7 closure document. One surviving root sustains
the same publisher; neither healthy root stops on-air authority and reaps the
encoder. Repair recovers only complete closed recording fragments. These
tests do not certify hung hardware, an arbitrary WAN or guaranteed RPO.
Restore the
paired disk before restart; startup never invents missing mount markers.
Finite no-JS rewind clips can now be regenerated from a fresh, owned replica
when the primary recording is absent. The builder pins only closed segments
in the selected five-minute portion with private hard links, remuxes once and
removes its pins afterward. It reserves 1 GiB from the shared native processing
pool before pinning/remuxing, has finite admission/build deadlines and rechecks
the exact current publication/session before responding. Both source and clip
have byte ceilings. These regenerable clips are not another durable recording;
this alone does not establish publisher disk failover or guaranteed RPO.

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
Paired disks are not a second serving node. The separate optional federation
copier serves acknowledged copies on an independent instance; it does not
turn a paired directory into a shared-database web cluster.

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
