# Engineering Principles

- Types over constants: use `size_of`, not magic numbers. If a type exists, reference it.
- Shared types belong in lockjaw-types, not duplicated across crates.
- No `unsafe` unless there is genuinely no alternative. Prefer locals and parameters over `static mut`.
- Every asm line commented.
- Delete dead code. Track deferred ideas in `docs/yagni-parking-lot.md`.
- Never remove existing comments during refactors. Verify they survive edits.
- Stage for Codex review before committing non-trivial changes.
- Instrument before guessing. Test bugs by construction.
- Small atomic commits for trackability.
- Never guess MMIO/IRQ values. Always dump and read the DTB.
- Single source of truth: don't copy logic between crates.
- Every bug fix and review fix needs a test that locks it down.
- Use drop guards for resource cleanup on failure paths, not manual rollback in each error branch.
- Push pure state and logic into lockjaw-types where possible so it can be tested on the host.
- QEMU requires dual UARTs + GICv3. Use the Makefile, not bare `-nographic`.

# Prioritization: correctness over speed

Lockjaw is a kernel written entirely in Rust. The goal is
architectural correctness and code isolation that humans cannot
achieve at this scale. Development speed is already 100x a human
team — use that leverage for correctness, not throughput.

The architecture is documented in three places:
- `docs/book-of-lockjaw/01-architecture.md` — philosophy and the
  push/pull/plan-apply taxonomy.
- `docs/patterns/` — technique catalog with canonical examples.
- `docs/extraction-roadmap.md` — what's left to extract, ranked.

When choosing what to work on next:
- **Prefer hard wins over fast wins.** The value is in making the
  architecture safer, not in inflating test counts with easy
  extractions.
- **Follow the push→pull rubric.** Convert the riskiest push-shaped
  kernel code to pull or plan/apply first. See `docs/patterns/` for
  the four shapes and how to recognize them.
- **Make illegal states unrepresentable.** If the type system can
  prevent a bug class, prefer that over runtime assertions. Narrow
  return types, distinct step variants, and construction-safe APIs
  are worth the design effort.
- **The kernel should be thin.** Inline assembly + mechanical
  execution of lockjaw-types decisions. Every decision that can be
  a pure function in types should be. The kernel's job is side
  effects, not policy.
