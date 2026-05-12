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

3. **Foundational substrate debt that distorts the kernel's mental
   model across multiple subsystems should be paid down ahead of
   feature work, even when local workarounds exist.** This is the
   prioritization meta-rule. Most debt waits its turn (see Tier 4
   #15 — "debt is stuff we need but can't build yet"). Substrate
   debt is the exception: it keeps the surface working but biases
   downstream design into the wrong shape, so polishing on top of
   it compounds the problem rather than progressing past it.

   A debt is substrate-level — and qualifies for the queue jump —
   when **multiple** of these signals fire strongly together (any
   one in isolation is too weak):

   - **Repeated compensating code in otherwise unrelated
     subsystems.** This is the loudest signal. If the same shim,
     the same `+OFFSET` arithmetic, or the same special-case
     match keeps appearing in unrelated kernel areas, the
     substrate is wrong, not the call sites.
   - **A `match kind { ... }` to pick which address regime,
     ownership rule, or lifecycle path applies.** The regime
     should be in the type, not in a runtime branch every reader
     has to re-derive.
   - It leaks into allocator design, address-space assumptions,
     ownership semantics, or object-model rules.
   - It makes platform bring-up or portability materially harder.
   - Fixing it would simplify or eliminate multiple downstream
     problems at once.

   Counterweight — substrate debt does *not* jump the queue when
   an in-flight feature is the forcing function that will clarify
   the right fix, or when a hard external deadline blocks the
   rework. In those cases, ship the feature, then pay down.

4. **Any correctness bug should come with both a fix to the bug
   and a suggestion for improving correctness by construction in
   the area, however expensive it may sound.**

## Tier 2 — Kernel Architecture

How tier 1 manifests in the kernel's shape.

5. **The kernel has to be strict and hardcore about construction
   so that user mode does not have to be as much so.**

6. **Reach for userspace before reaching for the kernel.**
   Every kernel feature must be justified by what it enables
   that userspace can't do alone. New `HandleKind` variants,
   new syscalls, new kernel-side dispatch paths, new kernel
   state — all last-resort. The kernel is for hardware (MMU,
   GIC, TTBRn registers), substrate (PageSets, KVM allocator,
   scheduler), capability creation, and IPC routing.
   Everything else is userspace.

   When this fights Tier 3's "use strong typing" idiom: let
   this principle win. Typed kernel cap variants enforcing
   what is really a userspace-driver protocol pushes policy
   into the kernel. Type the protocol where it lives — in
   `lockjaw-types` message shapes and `lockjaw-userlib`
   client wrappers — not in the cap layer. The existing
   `BlockEngine` / `DisplayEngine` pattern (typed traits +
   generic Endpoint handles) is the model.

7. **Don't allow state machines or non-trivial logic in kernel
   code — build pull mechanisms or plans in types.**

8. **Unsafe should be strictly used for doing asm, and for
   casting pages to Rust objects, and everything downstream
   should be safe.**

9. **Don't use unsafe types — make wrappers that force the
   unsafety to only be exposed at creation.**

## Tier 3 — Type-System Idioms

Specific Rust patterns that implement tiers 1 and 2.

10. **Use Rust per-enum values where you can to be correct by
    construction.**

11. **In general use strong typing wherever possible** — even
    if it does not survive the syscall boundary, it still
    enforces correctness.

12. **Use RAII wherever possible rather than rely on manual
    cleanup.**

## Tier 4 — Project & Process

Working rules that aren't directly about correctness.

13. **Drivers should be split between bit bangers and protocol
    drivers.** UART buffering should not be repeated everywhere,
    block management should not be repeated everywhere; think of
    KMD vs UMD in Windows.

14. **Prefer clean code and abstractions over feature richness.**

15. **Back compatibility is not a concern until much later** —
    we should feel free to refactor anything and change any
    protocol.

16. **YAGNI is for things you can build now but don't need; debt
    is stuff we need but can't build yet.** The Tier 1 #3
    counterpoint: substrate debt is need-but-can't-build-yet
    debt that *also* keeps the surface working — the surface
    working is not a reason to defer it.
