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
