# Lockjaw docs/

Living documentation for the Lockjaw kernel and userspace. Organized by what
the content is *for*, not when it was written.

- **architecture/** — durable design: four chapters of the "Book of Lockjaw"
  plus a technique catalog in `patterns/`. Read first.
- **reference/** — how the code works today. Must track HEAD. See the
  per-doc index below.
- **process/** — principles (`ben_principles.md`), reviewer-agent system
  prompts, and the orchestrator-side paired-review workflow.
- **tracking/** — living backlogs: `tech-debt.md` (needed but unbuilt),
  `yagni-parking-lot.md` (built but unneeded), `extraction-roadmap.md`,
  `kernel-vmem-roadmap.md`, `ben_todos.md`.
- **plans/** — active multi-phase plans. Currently `posix-musl-plan.md`.
- **journals/** — chronological narratives, one per phase. Preserved
  verbatim; read for the "why was this done" context behind a feature.
- **history/** — completed plans, preserved verbatim for design rationale +
  reviewer Q&A. "Done, not discarded."
- **references/** — external materials: BCM2711 datasheet, u-boot SDHCI
  source for cross-checking.
- **archive/** — shelved-but-revivable internal work (patches).

## reference/ index

Current-state references for the runtime surfaces a contributor will
touch. Grouped by topic; each tracks HEAD.

### Kernel core
- `memory-model.md` — buddy allocator + DMA pool + KVM higher-half pool, DTB-runtime sizing.
- `object-model.md` — PageSet → object lifecycle; HandleKind variants; rights bitmask.
- `ipc.md` — synchronous rendezvous IPC, the five syscalls, call/reply Reply objects, register conventions.
- `syscalls.md` — the 32-entry syscall table + the wait_any extended section.
- `threads.md` — context switch, GKL, BlockToken, scheduler shell.
- `scheduler.md` — pure model in lockjaw-types: SchedDecision / select_next / invariants / preemption points.
- `smp.md` — per-CPU stack layout, MPIDR / TPIDR / cpu_id triple, PSCI vs spin-table, GKL regime.
- `process-creation.md` — sys_create_process walkthrough, plan/apply orchestrator, teardown plan.
- `exception-handling.md` — AArch64 vector table beyond SVC; ESR decode.
- `boot-sequence.md` — `_start` to init at EL0, plus secondary CPU bringup.

### Memory / VM
- `higher-half-kernel.md` — TTBR0/TTBR1 split.
- `stack-budget.md` — 8 KiB per-CPU stack, the check-stack xtask discipline.

### Drivers + substrate
- `driver-howto.md` — step-by-step "write a new driver" walkthrough.
- `kernel-drivers.md` — why GIC + timer are the only kernel drivers; everything else is userspace.
- `regspec-format.md` — TOML format for `cargo xtask gen-regs`.
- `wirespec-format.md` — TOML format for `cargo xtask gen-wires`.
- `linker-symbol-audit.md` — linker-symbol-to-integer classification + the xtask that enforces it.

### IPC servers + protocols
- `device-manager-protocol.md` — 4 CMD codes for hardware claim/probe/release + ownership-by-token.
- `clock-subsystem.md` — two-leg clock IPC, ClockHandleTable, ClockClient.
- `partition-manager.md` — single-partition selector + BlockEngine sector translation.
- `fat32-server.md` — FS wire protocol + mount stack + cluster-chain walk.
- `musl-port.md` — patched headers + `lockjaw_syscall` shim + syscall routing.

If a doc you expect isn't where you look, `git log --follow` on the old path
will track it through the 2026-05 docs restructure.
