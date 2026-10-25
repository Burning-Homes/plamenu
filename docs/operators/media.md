# Media, music and processing

Plamenu uses persistent local media storage and PostgreSQL metadata. One serving
process owns the database; optional video processors do not turn it into a
multi-writer cluster. FFmpeg/ffprobe are needed for media processing. Packaged
OCI images include them; a binary host must install them.

## Capacity and quotas

Budget the media root, database, temporary workspace and backups separately.
Uploaded sources, encoded qualities, artwork, pending jobs and deletion debt can
coexist. A small final film does not imply a small processing reservation.
Set upload limits to what the filesystem can actually admit and monitor free
space during representative jobs; there is no certified universal minimum host.

| Configuration | Purpose |
| --- | --- |
| `[native_video]` | Source ceiling, per-account/global/pending storage, unfinished drafts, processing slots and disk floor |
| `[music]` | Retained audio, conversion bitrate/role caps, conversion scratch, ZIP jobs/sizes and retention |
| `[[music_local_roots]]` | Explicit operator-mounted libraries and write/delete capabilities |
| `[media_mirror]` | Optional paired durable media root |
| `[[video_offload]]` | Optional private processors, with local fallback |
| `[video_redundancy]` | Optional approved, consented copies of remote recorded videos |
| `video_analytics` | Server-wide permission for opt-in channel delivery aggregates |

Generate the current defaults with `config generate`; see
[configuration](configuration.md). Quota is released after acknowledged physical
deletion, not just removal of a database entry. Repair a failing cleanup worker
instead of manually reducing its charged bytes.

## Jobs and failures

**Administration → Music jobs** shows conversions, ZIP downloads, streams and
folder work. Viewing requires `view_devops`; stopping work requires
`manage_settings`. Cancellation stops the selected run, not its saved stream or
folder configuration. Watched folders can scan again. File recovery cannot be
cancelled halfway through a repair.

Video uploads and imports produce private drafts. Failed processing keeps the
source for retry; imports that already downloaded need not fetch it again.
Before raising concurrency, inspect CPU, disk bandwidth, free scratch and queue
age. Protect long upload/live responses from proxy idle/body limits appropriate
to your configured workload.

## Mounted music libraries

Declare an existing absolute root with an ID and label, then assign a root and
subfolder to a member in **Music → Local folders**. Indexing keeps originals in
place. Default source access is read-only; enable tag writeback, rename or delete
only if the member should modify those actual files.

Preserve the mount identity and ownership across restarts. Child symlinks and
path traversal are rejected. If a source operation reports recovery pending,
restore the expected mount/files and let recovery finish. Disconnecting a
library removes its index, not its originals. Back up source roots separately:
they are outside `media_dir` and must not be assumed present in a media archive.

## Paired media roots

Configure `[media_mirror]` with an already mounted, initially empty second root,
owned by the service account. Roots must be separate and non-nested. Preserve
pairing markers on both; do not recreate or delete them to bypass a missing disk.
On startup, existing durable primary objects are verified/seeded before serving.
This can be slow on a large store; allow for it during maintenance.

Durable writes and deletions acknowledge both roots. Reads can fall back to the
replica after normal authorization. Quota counts one logical object, but disks
need approximately two durable copies plus scratch. Two directories on one disk
are not separate failure domains. Replication also copies deletions: it is not
a backup or automatic repair of a corrupt primary.

Active live recordings have bounded checkpoints, including closed segments and
complete recording prefixes. On supported Linux paths, independent muxers can
continue on the surviving disk. A busy/failing replica can lag; watch
`live checkpoint degraded` and recovery messages. This does not guarantee zero
loss of the last on-air bytes. Never restore unrelated database/media generations
or treat a partial recording as a complete backup.

After an interruption, inspect broadcast and recording state. Recovery must
not advertise a dead encoder as live. Reconnect using the current management
credentials and verify the resulting session/recording. See
[backup and restore](backup-and-restore.md) for coordinated recovery.

## Private video processors

On the processing host, use the same release binary:

```sh
plamenu --config worker.toml config generate --video-worker
plamenu --config worker.toml video-worker
```

Protect the generated token. Configure the worker's dedicated scratch directory,
tools, budgets and TLS, then add its HTTPS origin/token to `[[video_offload]]`
on the main server. Plain HTTP is allowed only on literal loopback. Keep worker
and origin configuration private; never put tokens into public URLs.

The origin retains sources, database authority and publication. It independently
validates worker outputs and can fall back to local processing. Workers need no
database backup; keep enough origin capacity for fallback. Drain work before
changing both sides' token/configuration and restarting affected processes.

## Federation video copies

`[video_redundancy]` enables bounded copying of approved public/unlisted recorded
videos. Exact source and advertising domains require approval, and the creator
must permit copying or the operator must supply explicit consent evidence.
Download permission alone is insufficient. Live, encrypted or incomplete graphs
are not usable finite copies.

Manage approvals and jobs through the [operator API](../api/media-operations.md).
Expiry, withdrawal, source changes and policy revocation stop serving before
cleanup finishes. Copies never grant access to a private/deleted original.
Third-party players may still require the origin and may ignore redundancy
advertisements; do not promise universal playback through origin loss.

## Reconcile storage

Use `plamenu --config /etc/plamenu/plamenu.toml media reconcile` for a dry report.
Review it with a current backup before choosing deletion options. Do not delete
apparently unused media keys, pairing markers or job scratch by hand: pending
work and durable cleanup journals distinguish live files from orphans.
