# Ben's Engineering Principles

Personal principles for Lockjaw kernel work. Ordered by priority of
importance — apply higher-tier rules in service of lower-tier ones,
and use them to break ties when concerns conflict.

## Tier 1 — Prime Directives

The rules that decide which other rules to apply when there's
tension between them.

1. **Correctness by construction trumps ergonomics and velocity in
   kernel development every time.**

2. **When we find problems, find ways to fix the whole class of
   problem mechanically and by construction rather than just
   patching that one problem.**

3. **Any correctness bug should come with both a fix to the bug
   and a suggestion for improving correctness by construction in
   the area, however expensive it may sound.**

## Tier 2 — Kernel Architecture

How tier 1 manifests in the kernel's shape.

4. **The kernel has to be strict and hardcore about construction
   so that user mode does not have to be as much so.**

5. **Don't allow state machines or non-trivial logic in kernel
   code — build pull mechanisms or plans in types.**

6. **Unsafe should be strictly used for doing asm, and for
   casting pages to Rust objects, and everything downstream
   should be safe.**

7. **Don't use unsafe types — make wrappers that force the
   unsafety to only be exposed at creation.**

## Tier 3 — Type-System Idioms

Specific Rust patterns that implement tiers 1 and 2.

8. **Use Rust per-enum values where you can to be correct by
   construction.**

9. **In general use strong typing wherever possible** — even if
   it does not survive the syscall boundary, it still enforces
   correctness.

10. **Use RAII wherever possible rather than rely on manual
    cleanup.**

## Tier 4 — Project & Process

Working rules that aren't directly about correctness.

11. **Drivers should be split between bit bangers and protocol
    drivers.** UART buffering should not be repeated everywhere,
    block management should not be repeated everywhere; think of
    KMD vs UMD in Windows.

12. **Prefer clean code and abstractions over feature richness.**

13. **Back compatibility is not a concern until much later** — we
    should feel free to refactor anything and change any
    protocol.

14. **YAGNI is for things you can build now but don't need; debt
    is stuff we need but can't build yet.**
