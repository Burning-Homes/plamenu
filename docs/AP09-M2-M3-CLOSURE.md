# AP09 M2 / M3 closure record

Two additional milestones selected on 2026-09-29. Their original acceptance
gates are unchanged. This checklist records their functional verification,
not completion of the whole AP09 or independent release gates.

## M3 — accessible metadata

- [x] Atomic alternative-text/transcript/audio-description edits, including
      authorized collaborators and outbox failure rollback.
- [x] Explicit, bounded federation representation and update/clear retention;
      PeerTube's supported fields tested in both directions. Unsupported
      extension preservation by PeerTube must not be claimed.
- [x] Caption upload/edit/delete/languages, native tracks/cues and readable,
      seekable transcripts across the seven browser projects.
- [x] Chapters and descriptions authorable and usable with keyboard/no JS;
      safe text rendering, mobile reflow, reduced motion and forced colours.
- [x] Rust and real PeerTube regression matrix, evidence and user/API guide.
- [x] Sign-off 2026-09-29: see M3 record in AP09-VIDEO-PROGRESS.md.
      Human AT / WCAG release checks remain explicitly in M9.

## M2 — long-form VOD pipeline

Closed 2026-09-29 against the original functional gate. See the M2 sign-off in
AP09-VIDEO-PROGRESS.md for the complete evidence and explicit release boundaries.

- [x] Broader bounded rendition ladder, copy/remux top, progressive fallback,
      actual PeerTube import and quality/seek browser evidence.
- [x] Published poster replacement and seek previews/storyboards with native
      HTML fallback, immutable assets, quotas and update federation.
- [x] Large multipart/resumable upload measurements, restart/recovery,
      concurrency/admission/fairness, actual duration and codec checks.
- [x] Audio-only/silent and difficult inputs, cleanup/cancellation/failure,
      transactional rendition publication and quota evidence.
- [x] Rust, PeerTube, seven-project browser, resource measurements and guide.
- [x] Sign-off. Broader operations/scale and manual release gates remain M7/M9.

### Resource evidence — 2026-09-29

Measured on the development host: Intel i7-11700F, 8 cores / 16 threads,
31 GiB RAM, workspace-backed media storage, two native worker slots.
Memory values are rounded MiB peaks for these workloads, not production capacity claims.

| Workload | Transfer / total | Server RSS growth | Encoder children peak |
| --- | --- | --- | --- |
| 2 GiB multipart, decodable hour | 8.98 / 11.61 s | 15 MiB | 56 MiB |
| 2 GiB resumable, kill/resume and processing | 12.37 / 13.98 s | 60 MiB | 57 MiB |
| Three concurrent 128 MiB uploads, two uploaders | 16.91 s total | 28 MiB | 419 MiB |

The hourly sources contain a real 320×240, 1 fps, 3600-frame silent H.264
film plus valid ISO-BMFF `free` padding. This isolates maximum transfer size
from encoding complexity. It is not a claim that an hour of 1080p complexity
encodes in fourteen seconds. Independent tests exercise a real five-rung
1080p ladder and three concurrent 720p/15 fps, 30-second MPEG-4 sources that
require H.264 encoding. The concurrent run observes two processing jobs and
two active uploaders; every source remains privately decodable in full.

Both maximum-size modes reject an above-ceiling declaration before source
transfer, survive a process kill/restart, and cancel with zero ledger debt
only after acknowledged file deletion. The resumable case preserves its
committed offset and same-file fingerprint across the interrupted upload.
Initial concurrency measurement exposed 915 MiB encoder RSS from unrestricted
FFmpeg thread counts; bounded per-slot codec shares reduced it to 419 MiB.
Admission remains conservative and is not relaxed to fit the test fixture:
media was moved from the small RAM-backed `/tmp` mount to the workspace disk.

Reports and preserved failures: `target/e2e/ap09-m2-20260929/`.

No push, merge or staging deployment is authorized for this continuation.
