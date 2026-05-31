# Development Journals

Chronological narratives of Lockjaw development, one per phase. Each journal
was written contemporaneously, after a milestone, and is preserved as-written.
Stale "What's next" sections inside a journal are part of the snapshot — not
bugs.

Filename note: `development-journal.md` is journal 1 (the original, written
when there was no journal series yet). Journals 2 through 11 follow as
`development-journal-N.md`.

| File | Span |
|---|---|
| `development-journal.md` | Phases 1-6 — boot through EL0 preemptive scheduling |
| `development-journal-2.md` | Phases 7-8 — IPC state machine + userspace ELF process creation |
| `development-journal-3.md` | Original-plan Phase 9 — ramfb display driver + unsafe reduction |
| `development-journal-4.md` | Typed-wrapper series + offset-arithmetic cleanup |
| `development-journal-5.md` | Display DDI — capability gap fix, typed handles, drop guards |
| `development-journal-6.md` | PageSet lifecycle + push/pull/plan-apply taxonomy formalized |
| `development-journal-7.md` | Architecture catalog docs + scratch-buffer refactor + POSIX Phase 0 |
| `development-journal-8.md` | POSIX Phase 1 — FAT32 server, three-server chain, musl gate |
| `development-journal-9.md` | POSIX Phase 2 mmap gate + handle revocation + variable-size PageSets |
| `development-journal-10.md` | Kernel objects → KVA migration + kernel image relink + Pi 4B first boot |
| `development-journal-11.md` | Typed-MMIO substrate Phases 9-10 — DMA coherence envelope + reviewer loop |

Spans correspond roughly to the Apr 2026 → May 2026 commit window. The
journal-numbering convention is loose: a journal closes a milestone, not a
fixed time window.
