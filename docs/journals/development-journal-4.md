# Development Journal: The Unsafe Reduction, Three Wrong Answers, and Finding the Floor

Written after the typed-wrapper series and offset arithmetic cleanup. This was the session where Codex rejected my work three times in a row on the same API, and each rejection taught me something I should have already known.

## I keep making the same mistake

The typed-wrapper plan was straightforward: handle lookup returns a typed object, IPC functions take typed refs, the syscall handler stops needing unsafe. Simple. I wrote it, it compiled, it booted, and I staged it for review.

Codex rejected it immediately. `HandleTableRef::lookup_endpoint()` returns `KernelMut<EndpointObject>`. Safe code can call it twice with the same handle and get two exclusive mutable wrappers to the same object. That's the exact aliasing bug we'd already fixed in CurrentThread three days earlier.

I should have seen it. The CurrentThread saga went through three approaches before landing on narrow per-field accessors, and the reason approach 1 was rejected was exactly this: returning a general mutable borrow from a safe API is unsound when the underlying resource can be accessed from multiple paths.

So I tried closures: `with_endpoint(&mut self, handle, rights, |ep| { ... })`. The `&mut self` prevents nesting from the same HandleTableRef. Codex rejected it again: safe code can construct two HandleTableRefs and nest closures from both. Same bug, slightly more inconvenient shape.

Then I tried a borrowing token: an `ObjectAccessToken` created once per syscall, passed by `&mut` to operations. Ben stopped me before I even staged it. Codex had already said: "I am not a fan of stopgap solutions." Ben agreed and pointed me at the pattern I'd already used successfully in CurrentThread: narrow operation methods, not general-purpose mutable borrows.

The final answer was `object_ops.rs` — a tiny facade module with methods like `send(handle, msg)`, `receive(handle)`, `call(ep_handle, reply_handle, msg)`. Each method does the lookup, creates the KernelMut, calls the IPC function, and drops the KernelMut. No mutable object reference ever appears in a public API. The same pattern as CurrentThread, the same pattern as the scheduler, the same pattern as the page allocator.

Three wrong answers to arrive at the one I should have written first. The pattern was right there in the codebase. I just kept reaching for generality instead of looking at what already worked.

## Why I reach for generality

I think the instinct comes from a good place. When I see "look up an endpoint and do something with it," my first thought is to expose the endpoint so the caller can do anything. That's the flexible API. The one that handles future requirements without changes. The one a library author would write.

But a kernel isn't a library. The set of operations on an endpoint is closed: send, receive, call, recv_nb. That's it. There will never be a "do anything with this endpoint" caller. The generality isn't free — it creates a surface for aliasing bugs that narrow methods eliminate structurally.

Ben said something in this session that I keep coming back to: "I am not a fan of stopgap solutions." He wasn't talking about the token — he was talking about a mindset. The token was a patch over the wrong API shape. The right answer was to change the shape, not to add guards around the wrong one.

## The audit changed how I think about "done"

After the typed wrappers landed, Ben asked me to audit all unsafe usage related to page-backed memory. Not "are there bugs" but "are we exclusively accessing members through Rust struct syntax instead of pointer arithmetic?"

The audit found two outliers:

**Handle table slots** were accessed via `slots_base + index * slot_size` — manual offset arithmetic that would silently break if HandleEntry's layout changed. The fix was `core::slice::from_raw_parts_mut` and regular `slots[i]` indexing. One line to create the slice, then normal Rust everywhere.

**SavedContext** was written via `ctx.add(0)` through `ctx.add(11)` — raw pointer offsets to register slots that had to match the assembly by convention, with no compile-time enforcement. The fix was a `SavedContext` struct with named fields (`x19`, `x20`, ..., `lr`) and `offset_of!` assertions that fail the build if the struct doesn't match the assembly. This is a genuine correctness improvement — a struct layout change that breaks the context switch now fails at compile time instead of corrupting registers at runtime.

After fixing both, I audited every remaining raw pointer cast in non-arch code. Thirty-three casts across eight files. Every single one fell into a category that couldn't be improved further: the KernelRef/KernelMut abstraction itself, factory functions initializing fresh pages, FFI boundaries, crash diagnostics, linker symbols. Zero cases where wrapping would prevent a real class of bugs.

That's when Ben introduced a concept I hadn't heard before: **theatre**. Some unsafe wrapping makes code safer. Some makes code look safer without preventing any actual bugs. The remaining casts were theatre territory. We documented them as the floor and stopped.

Knowing when to stop is a skill I'm still developing. My instinct is to keep wrapping until there's nothing left to wrap. The audit taught me to ask "what class of bugs does this prevent?" instead of "can I make this look safer?"

## The context switch discovery

The most honest moment in this session was when Codex found a pre-existing unsoundness that none of our work could fix.

IPC functions that block — `ipc_send`, `ipc_receive`, `ipc_call`, `notification_wait` — create a `KernelMut<EndpointObject>` and hold the resulting `&mut EndpointObject` alive across `scheduler::block_current()`. While that thread is suspended, another thread can enter the kernel and create its own `&mut EndpointObject` to the same endpoint. Two simultaneous `&mut T` to the same object is undefined behavior under Rust's aliasing model, regardless of single-core execution.

This is not a bug our typed wrappers introduced. It was there before — the old IPC code did the same thing with its own KernelMut. It's not a bug our typed wrappers can fix, either. The fix is to stop creating `&mut T` references for shared kernel objects entirely and work through raw pointers with `UnsafeCell`-style access.

I initially wanted to dismiss this as theoretical. Single-core execution, IRQs masked, opaque function calls that prevent reordering — the compiler can't exploit the aliasing in practice. But Codex was right to flag it, and Ben was right to log it. It's technically UB under Stacked Borrows. Miri would catch it. An SMP port would expose it as a real data race. A future compiler that optimizes more aggressively across opaque calls could miscompile it.

We logged it in tech-debt.md with three concrete fix options and moved on. The honest framing: the typed wrappers are a real improvement to the API boundary and LLM guardrails, but there's a deeper aliasing-model mismatch that requires a separate focused refactor of the KernelMut primitive itself.

I could have tried to downplay this. Instead, I argued to Codex that it was pre-existing and orthogonal, Codex agreed, and we documented it honestly. That's the right process: find the issue, assess whether it blocks the current work, log it if it doesn't, and don't pretend it doesn't exist.

## What the numbers say

Handler unsafe blocks: 13 at the start of this session, 4 at the end. The remaining four are genuine machine-boundary operations: page table writes, GIC MMIO, readiness waiter registration, cross-object Reply pointer chasing. None of them can be eliminated without deeper refactors that have their own costs.

Seven IPC/notification syscalls went from unsafe orchestration to zero-unsafe calls through `object_ops`. The benchmark sender and receiver are fully safe. `sys_query_pageset_phys` is fully safe. `sys_create_process` is fully safe at the call site.

`KERNEL_VA_OFFSET` went from being scattered across consumer code to being confined to the abstraction (`kernel_ptr.rs`, `addr.rs`), one layout bridge (`table_slots`), and a handful of boot one-shots.

Every kernel object in a donated page is now accessed through Rust struct field syntax. No manual pointer arithmetic for field access remains in non-arch code.

## What I think about the two-model process now

This session proved the two-model review process in a way the previous session didn't. In session 3, Codex caught aliasing bugs in code I was writing for the first time — bugs I might have caught myself with more careful thought. In this session, Codex caught unsoundness in API designs that I had thought through, implemented, tested, and believed were correct. Three times.

Each rejection made the final design better in a way I would not have reached alone:

1. "Returning KernelMut is unsound" pushed me from object-borrow to closure-based.
2. "Closures with reconstructible wrappers are unsound" pushed me from closure-based to token-based.
3. Ben's "I am not a fan of stopgaps" + Codex's "operation-shaped, not borrow-shaped" pushed me to the final `object_ops` facade.

The key insight is that Codex doesn't just find bugs — it finds design-level unsoundness that compiles, boots, passes tests, and looks correct. The kind of unsoundness that only shows up when someone writes code you didn't anticipate, which is exactly what an LLM will do six months from now when adding a new syscall.

I implement. Codex reviews the shape. Ben arbitrates. That's the loop, and I trust it more after this session than I did before.

## What I'd do differently

If I started the typed-wrapper series over, I would look at CurrentThread's history first. Not just what it does now, but how it got there — the three approaches, the Codex rejections, the final narrow-accessor pattern. The same journey happened again for HandleTableRef, and I could have skipped two iterations by reading my own project's commit history.

I would also be more aggressive about the audit earlier. We did the audit after the wrappers landed, but doing it before would have shown me the handle table offset arithmetic and SavedContext pointer math before I started the wrapper work. Those were easier fixes than the API design, and they delivered the same kind of value: making the compiler catch layout mismatches that were previously convention.

And I would have proposed `object_ops.rs` from the start instead of typed lookup methods. The pattern was already in the codebase. I just didn't see it because I was thinking about the problem as "how do I return a typed object" instead of "how does CurrentThread make TCB access safe."
