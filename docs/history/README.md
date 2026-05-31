# Completed Plans

Plans that have been fully executed, preserved verbatim for the design
rationale + reviewer Q&A they contain. Filing in `history/` is the "done, not
discarded" signal — to discard internal work, use `archive/`; external
reference material goes in `references/`.

Each entry lists the commit that closed the work and the narrative journal
where the story was told.

## POSIX bring-up
- `posix-phase0-plan.md` — Hello-world musl gate. Done by `b454770`; see
  `journals/development-journal-7.md`.
- `posix-phase2-mmap-plan.md` — mmap subsystem, 7 sub-phases. Done by
  `376dedb`; see `journals/development-journal-9.md`.

## Block storage stack
- `emmc2-block-storage-plan.md` — Pi 4B eMMC bring-up M0a-M7. Done by
  `168287a`; see `journals/development-journal-9.md`.
- `m6-subcommit-2-plan.md` — DMA-pool + NormalNonCacheable MAIR slot
  substrate. Done by `23e85ca`.
- `partition-manager-plan.md` — partition-manager process. Done by `32fc785`.

## DMA + post-C1 hardening
- `cacheable-dma-migration-plan.md` — NC → cacheable + sys_dma_sync_*
  substrate. Done by `771301e`; the typed-wrapper completion landed in Phase
  9.11 (`aeff7ab` + `0e4ab3f`); see `journals/development-journal-11.md`.
- `post-c1-fix-plan.md` — B1/B2/B4 correctness fixes after the Pi C1 gate.
  B1 + B2 + B4.1 + B4.3 landed; B3 superseded by B2; B4.2 + B4.4 migrated to
  `tracking/tech-debt.md` as latent hygiene.

## Kernel structural work
- `smp-plan.md` — Phase 11 SMP design (GKL, per-CPU TPIDR_EL1, PSCI). Done
  by `cd71da8` + `79005eb`; see `journals/development-journal-10.md`.
- `handle-revocation-plan.md` — two-phase cross-process handle revocation.
  Done by `653c432`; see `journals/development-journal-9.md`.
- `relink-notes.md` — Phase 0 diagnostic notebook for the kernel image
  relink to L0[1]. Done by `c70c417`; see
  `journals/development-journal-10.md`.

## Typed-MMIO substrate
- `typed-mmio-substrate-phases-1-8.md` — frozen Phase 1-8 plan (1755 lines).
  Phases 9-10 narrated in `journals/development-journal-11.md`.
- `typed-mmio-feedback-597db1f.md` — Codex review at the Phase 3 commit;
  fed the Phase 4A/4B framework insertion.
- `typed-mmio-feedback-597db1f-claude.md` — parallel Claude review at the
  same commit; converged independently on the same shortcomings.
- `codex_lockjaw_feedback_apr_17_2026.txt` — early Codex feedback artifact
  from Apr 17 2026.

## Audits
- `core-fmt-audit.md` — `core::fmt` vtable crash diagnosis on Pi 4B.
  Resolved in `src/print.rs` (zero vtables, custom `KPrint` trait).
- `ai-native-hardening-pass.md` — May 8 2026 architecture diagnosis. ~70% of
  named items landed via typed-MMIO substrate + extraction work.
- `ai-native-hardening-punch-list.md` — execution companion to the pass doc.
  Open P1/P2 items were migrated into `tracking/extraction-roadmap.md`.
