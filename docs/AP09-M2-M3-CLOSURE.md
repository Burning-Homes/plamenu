# AP09 M2 / M3 closure record

Two additional milestones selected on 2026-09-29. Their original acceptance
gates are unchanged. This record is a checklist, not a completion claim.

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

- [ ] Broader bounded rendition ladder, copy/remux top, progressive fallback,
      actual PeerTube import and quality/seek browser evidence.
- [ ] Published poster replacement and seek previews/storyboards with native
      HTML fallback, immutable assets, quotas and update federation.
- [ ] Large multipart/resumable upload measurements, restart/recovery,
      concurrency/admission/fairness, actual duration and codec checks.
- [ ] Audio-only/silent and difficult inputs, cleanup/cancellation/failure,
      transactional rendition publication and quota evidence.
- [ ] Rust, PeerTube, seven-project browser, resource measurements and guide.
- [ ] Sign-off. Broader operations/scale and manual release gates remain M7/M9.

No push, merge or staging deployment is authorized for this continuation.
