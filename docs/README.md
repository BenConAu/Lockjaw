# Lockjaw docs/

Living documentation for the Lockjaw kernel and userspace. Organized by what
the content is *for*, not when it was written.

- **architecture/** — durable design: four chapters of the "Book of Lockjaw"
  plus a technique catalog in `patterns/`. Read first.
- **reference/** — how the code works today: memory model, IPC, syscalls,
  scheduler, register/wire formats, driver substrate. Must track HEAD.
- **process/** — principles (ben_principles.md), reviewer-agent system
  prompts, and the orchestrator-side paired-review workflow.
- **tracking/** — living backlogs: tech-debt (needed but unbuilt),
  yagni-parking-lot (built but unneeded), extraction-roadmap,
  kernel-vmem-roadmap, ben_todos.
- **plans/** — active multi-phase plans. Currently `posix-musl-plan.md`.
- **journals/** — chronological narratives, one per phase. Preserved
  verbatim; read for the "why was this done" context behind a feature.
- **history/** — completed plans, preserved verbatim for design rationale +
  reviewer Q&A. "Done, not discarded."
- **references/** — external materials: BCM2711 datasheet, u-boot SDHCI
  source for cross-checking.
- **archive/** — shelved-but-revivable internal work (patches).

If a doc you expect isn't where you look, `git log --follow` on the old path
will track it through the 2026-05 docs restructure.
