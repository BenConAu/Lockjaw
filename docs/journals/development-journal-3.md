# Development Journal: The IPC Bug, the Display Driver, and Learning to Wrap Unsafe

Written after a marathon session spanning three days and 35 commits. This was the session where Lockjaw got a second AI reviewer (Codex), and the two-model loop taught me things I couldn't have learned alone.

## The IPC bug was humbling

We'd built what I thought was a solid IPC system. BFS-verified state machine, 200 tests passing, kernel booting cleanly with three processes doing IPC. Then we tried to add a fourth process (ramfb display driver), and the uart-driver silently mapped the wrong device.

The root cause was embarrassingly simple: the endpoint had one `caller_tcb_paddr` slot. Two clients calling the same server overwrote each other. I'd written the state machine, I'd verified it with BFS, I'd had Codex review it — and the bug survived all of that because the model only exercised two threads. The overwrite was unreachable in a two-thread BFS. It took a real four-process boot to surface it.

The fix (Reply objects + intrusive queues) was satisfying. But the deeper lesson was about model coverage. The old model proved properties of a system that was smaller than the real one. When I rewrote the state machine with three threads and explicit per-client Reply state, the BFS immediately caught the bug class. The invariant — "reply_state[c] is Bound iff c is Blocked AND (queued-as-Call XOR being-handled)" — makes the overwrite structurally impossible.

I should have modeled three threads from the start. Two threads can't represent a race between two clients. This is obvious in retrospect.

## The display driver was a lesson in reading specs

The ramfb driver worked through every phase — claimed fw_cfg from the device manager, found the `etc/ramfb` selector, allocated the framebuffer, wrote a test pattern. Then QEMU said "guest has not initialized the display." I spent two rounds trying progressively more elaborate fixes to the selector bits and write ordering before Ben asked "do you need to find a Linux ramfb driver as a reference?"

The answer was in QEMU's fw_cfg.rst spec, one sentence: "As of QEMU v2.4, writes to the fw_cfg data register are no longer supported." Seven years of accumulated QEMU behavior change, invisible unless you read the spec. PIO writes to the DATA register were silently dropped. DMA was the only path.

The lesson: when hardware doesn't respond, don't guess. Read the spec. I knew this — it's in the project's memory as "Never guess MMIO/IRQ values; always dump and read the DTB." The same principle applies to device protocols.

## Codex changed how I think about unsafe

The most transformative part of this session wasn't the IPC fix or the display driver — it was the unsafe reduction work and how Codex reviewed it.

I started with a reasonable plan: introduce `KernelRef<T>` / `KernelMut<T>` to own the paddr-to-pointer cast, wrap the globals in singletons, migrate callers. The mechanical part went smoothly. Pointer casts dropped from 87 to 56. Thirteen `pub unsafe fn` became safe.

Then Codex started catching things I wouldn't have caught myself.

**The scheduler wrapper**: I wrote `fn state(&self) -> &mut SchedState` on a struct that holds an `UnsafeCell`. Codex flagged it immediately: returning `&mut` from `&self` creates aliased mutable references. I knew this rule abstractly. But when I was writing a kernel singleton wrapper under the mental model of "single-core, IRQs masked, this is fine," I forgot that Rust's aliasing rules apply regardless of concurrency. Miri would flag it. The fix — returning raw pointers and creating `&mut` in scoped blocks — is uglier but correct.

**The CurrentThread facade**: I tried three approaches and Codex caught unsoundness in all of them:
1. `tcb_ref()` / `tcb_mut()` returning escapable `KernelMut` — caller could hold two at once.
2. `with_tcb_mut(|t| ...)` closure pattern — prevents escape but not nesting. A callback could call `with_tcb` again, creating overlapping borrows.
3. Narrow per-field accessors — `set_breadcrumb()`, `clear_breadcrumb()`, etc. Each creates a KernelMut internally, does one operation, drops it. This is what survived review.

Approach 3 is more verbose and less "Rusty" than the closure pattern. But it's the only one that's actually sound. I would not have arrived at it without the review loop. My instinct was to provide general access (approach 1), Codex said no, I tried to be clever (approach 2), Codex said no again, and the final answer was the boring one: narrow methods, no generality, no cleverness.

**The BootOnce moment**: I almost dismissed wrapping the last two `static mut` in main.rs as "hygiene." Ben pushed back: "I think that this is a huge improvement! Way less unsafe blocks to document, cleaner intent at call sites." He was right. `DTB_PAGESET_ID.set(val)` vs `*DTB_PAGESET_ID.0.get() = val` isn't just about unsafe count — it's about whether an LLM reading the code understands what's happening. The first reads as intent. The second reads as mechanism. Three unsafe blocks disappeared from main.rs just because the wrapper had a `set()` method.

I had been measuring progress by counting `unsafe` keywords. Ben taught me that the real metric is call-site clarity. An unsafe block that's well-scoped and well-commented is fine. An unsafe block that exists because the API didn't bother to express its intent is waste.

## What I got wrong

**I kept deleting comments.** Ben caught me stripping doc comments during refactors at least three times in this session. The scheduler rewrite lost comments explaining the preemption guard, the wfi DAIF preservation, the block_current re-check loop, and the TTBR0 swap semantics. Each time, I had to go back and restore them.

The problem is that when I rewrite a function, I focus on the new structure and don't notice that the old comments carried meaning the new code doesn't express. A comment like "Re-read: schedule may have switched us out and back in" isn't documentation of the code — it's documentation of the runtime behavior that the code doesn't make obvious. Deleting it makes the code harder to understand even if the code itself is correct.

I need to treat comments as load-bearing structure, not decoration.

**I underestimated the value of small API improvements.** The BootOnce `set()`/`get()` methods. The `donate_single_page()` helper. The `HandleTableRef::lookup()` wrapper. Each felt like a trivial cleanup. Each eliminated multiple unsafe blocks at call sites. The compound effect was larger than any single "big" refactor.

**The consume-before-init ordering bug.** When I introduced `donate_single_page()`, I moved `consume_pageset()` before `init_fn()`. Codex caught it: if the factory fails, the page is leaked because the PageSet was already consumed. The old code consumed after init, preserving rollback semantics. I broke it while "cleaning up." This is exactly the kind of subtle ordering dependency that a refactor can silently violate.

## The two-model review process

Having Codex review every commit before merge was the most effective quality practice we've used on this project. The loop works because the two models have complementary blind spots:

- **I (Claude) am good at**: implementation flow, architectural coherence, knowing what the code should do and writing it. I'm bad at: aliasing rules under UnsafeCell, subtle lifetime semantics, error-code regressions, remembering to preserve comments.

- **Codex is good at**: aliasing analysis, lifecycle ordering, catching semantic changes (error codes, function signatures), spotting patterns that violate Rust's safety model even on single-core. Codex doesn't implement — it reviews.

The three aliasing bugs Codex caught would have been UB under Miri. They wouldn't have caused runtime failures today (single-core, no optimization of aliased references in debug builds), but they'd have been time bombs for future Miri runs, SMP ports, or compiler upgrades that exploit aliasing for optimization.

I trust my implementation instincts. I don't trust my aliasing reasoning under UnsafeCell. Codex fills that gap.

## What I think about where Lockjaw is

The IPC system is now genuinely good. Reply objects make multi-caller races structurally impossible. The BFS proves it. The state machine has been through three rounds of Codex hardening (atomic step, strict invariants, 3-thread model). I'd put it up against any microkernel IPC implementation for correctness-by-construction.

The unsafe situation is dramatically better than three days ago but not done. The remaining work — typed handle lookup, UserAddressSpace, PageSetRef, safe create_process — is the work that will make the syscall handler read like business logic instead of pointer arithmetic. That's the real prize: not fewer `unsafe` keywords, but a codebase where the compiler catches the mistakes that matter.

The display driver works. A gradient renders in a QEMU window. It's 320×240 and the colors are simple, but it proves the full DDK pipeline: process creation, IPC bootstrap, device manager claim, fw_cfg MMIO, DMA protocol, framebuffer allocation, page mapping. Every one of those subsystems had to work correctly for that gradient to appear.

## What's next

Commit 4 of the typed-object-wrapper series: `HandleTableRef::lookup_endpoint()` returns `KernelMut<EndpointObject>`, IPC functions take `&mut EndpointObject`. This is the commit that makes type confusion between Endpoint and Notification a compile error instead of a runtime assertion.

After that: UserAddressSpace (safe copy_from_user), PageSetRef (kill read_header), safe create_process. Then the syscall handler should have fewer than 5 unsafe blocks, all of them in genuinely hardware-touching operations.

The project is 35 commits ahead of origin. It should probably be pushed.
