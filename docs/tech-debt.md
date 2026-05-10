# Technical Debt Log

Known limitations introduced for bootstrapping. Each item documents what we did, why, and what the eventual fix is. Unlike the YAGNI parking lot (removed code to add later), this tracks code that exists but is limited.

---

## Static IRQ binding table (96-slot cap)

**Where:** `src/arch/aarch64/irq_bind.rs`

**What:** IRQ-to-Notification bindings stored in a static array of 96 entries (raised from 32 once UART + virtio-blk + virtio-mmio device probing started filling it). No unbind, no conflict detection, no ownership tracking. The kernel still owns the table directly even though the device-manager process is now the policy authority for who gets which device.

**Why bootstrap:** A single UART IRQ binding doesn't need more, and the device manager + drivers we have today fit under 96. Moving the table into the device manager would require an IPC round-trip on every bind.

**Fix:** Move the binding table into the device manager process; the kernel syscall stays simple (one binding at a time), and the device manager arbitrates ownership and detects conflicts.

---

## Kernel identity map in user TTBR0

**Where:** `src/arch/aarch64/vmem.rs` (create_address_space, L1[1] and L2[4])

**What:** Every user address space includes the kernel's physical address range (RAM at L1[1], device MMIO at L2[4]) with AP_RW_EL1. Despite the higher-half kernel pivot via TTBR1, the kernel binary is still linked at physical addresses (`linker.ld` ORIGIN = `0x40200000`); the higher-half mapping is an additional view of the same physical memory, not a relocation. Some kernel exception-handling paths still reference TTBR0-range addresses.

**Why bootstrap:** Relinking the kernel at higher-half VAs requires changing the linker script origin, adding a boot trampoline that runs at physical addresses before jumping to higher-half, and updating every function/static address. Significant change.

**Fix:** Relink the kernel at higher-half VAs (`linker.ld` ORIGIN = `0xFFFF_0000_4008_0000`). Add a boot trampoline in `boot.rs` that identity-maps initially, then jumps to higher-half after TTBR1 is installed. After that, TTBR0 can be pure user pages with no kernel entries. This unblocks the "kernel threads leave stale user TTBR0" item below.

---

## Syscall handler code repetition

**Where:** `src/syscall/handler.rs`

**What:** The `lookup_handle` helper was extracted after noticing the same 10-line pattern copied across 8 syscall handlers. Other patterns may still be repeated — a systematic scrub has not been done.

**Why:** Each syscall was added incrementally, one per milestone. Code was written for the immediate need without looking back at what could be shared.

**Fix:** Periodically scrub the syscall handler for repeated patterns and extract helpers. The lookup_handle refactor is the model for how to do this.

---

## MAX_THREADS = 16 static scheduler array

**Where:** `src/sched/scheduler.rs:15`

**What:** The run queue is a fixed-size array of 16 slots (raised from 8). Cannot grow. `add_thread` returns false when full. Sufficient for today's ~11 active processes plus kernel threads, but a hard ceiling.

**Why bootstrap:** A dynamic run queue requires either kernel allocation (violates our principle) or a user-donated page for the queue.

**Fix:** User-donated page for the run queue, similar to how handle tables work. The scheduler would be initialized with a donated page that holds the thread array.

---

## UnsafeCell globals serialized only by GKL

**Where:** `src/cap/pageset_table.rs`, `src/arch/aarch64/irq_bind.rs`

**What:** Kernel globals use `UnsafeCell` with a manual `unsafe impl Sync` and a comment claiming "single-core kernel, no concurrent access." The comments are stale: SMP support landed (Phase 11), and the safety actually comes from the Giant Kernel Lock (`src/sched/gkl.rs`) which serializes all kernel-mode execution. The bare `UnsafeCell` is correct under GKL but the SAFETY comments lie about why.

**Why bootstrap:** Proper per-object locking would let cores execute non-conflicting kernel work in parallel; GKL is the placeholder until then.

**Fix:** Update the SAFETY comments to cite GKL (small fix, keeps current semantics correct on paper). Then, when the kernel grows beyond GKL — replace bare `UnsafeCell` wrappers with a kernel spinlock type (e.g. `SpinMutex<T>` that disables IRQs on lock) and audit every `unsafe impl Sync` for the same pattern.

---

## No safe page abstraction in userspace

**Where:** `user/lockjaw-userlib/src/lib.rs` (`zero_page_at_va`)

**What:** The kernel's `zero_page(PhysAddr)` is safe because `PhysAddr` is a trusted type from the page allocator. The userspace equivalent `zero_page_at_va(u64)` is unsafe because user virtual addresses are raw `u64` values with no type-level proof that they're mapped.

**Why bootstrap:** Userspace has no validated address types. Adding a `MappedPage` type returned by `sys_map_pages` would make page operations safe, but requires designing the userspace memory management API.

**Fix:** Introduce a `MappedPage` type in lockjaw-userlib that wraps a VA returned by a successful `sys_map_pages`. `zero_page_at_va` and other page operations would take `MappedPage` instead of raw `u64`, making them safe.

---

## Single-handle copy at process creation

**Where:** `src/process.rs:98-109`, `src/syscall/handler.rs` (sys_create_process x5 parameter)

**What:** sys_create_process copies at most one handle from the parent's handle table into the child's. x5 = handle index to copy, or u64::MAX for none. PageSet kinds are now explicitly rejected here (see the validate-phase check in `process.rs:98-109`) — callers that need to transfer a PageSet must use `sys_export_handle` instead.

**Why bootstrap:** The UART driver needs exactly one handle (the IPC endpoint from init). One handle is enough for current bootstrap shapes; everything else flows through `sys_export_handle` post-spawn.

**Fix:** Either extend to an array of handles (x5 = pointer to handle index array, x6 = count), or treat the current single-handle path as deprecated and route everything through IPC-based transfer. The latter is the proper microkernel approach — capabilities flow through IPC, not just at creation time.

---

## Kernel threads leave stale user TTBR0 in hardware

**Where:** `src/sched/scheduler.rs:435-442`

**What:** Kernel threads are created with `ttbr0_paddr = 0`. The scheduler only writes TTBR0_EL1 when `new_ttbr0 != 0`. When switching from a user process to a kernel thread, the previous user process's page table stays in TTBR0_EL1. An accidental lower-half access from a kernel thread would hit the previous process's memory instead of faulting cleanly.

**Why bootstrap:** Kernel threads only access higher-half addresses (via TTBR1). No kernel code accidentally touches the lower half. The risk is latent.

**Attempted fix (shelved):** Always write TTBR0 on context switch — `EMPTY_USER_L0` for kernel threads, user's table for user threads — with a conditional TLB flush only on kernel→user transitions to avoid a ~20x boot slowdown from per-switch `tlbi vmalle1is`. Patch archived at `docs/archive/fix2-ttbr0-always-write.patch`. Set aside because it still leaves a residual TLB-hit correctness gap and because the directional fix (below) subsumes it.

**Fix:** Remove the kernel's dependency on lower-half VAs entirely. Once the kernel is relinked at higher-half (see "Kernel identity map in user TTBR0") and no kernel code references lower-half addresses, TTBR0 becomes purely user-owned: the scheduler can leave it alone on kernel-thread switches without any correctness concern, and the TLB flush only needs to happen at user↔user transitions.

---

## BlockToken discipline is opt-in

**Where:** `src/sched/scheduler.rs` (BlockToken, scoped_mut), consumed by `src/ipc/endpoint.rs`, `src/ipc/notification.rs`

**What:** The BlockToken + scoped_mut pattern enforces at compile time that no `&mut T` reference to a shared kernel object survives across `block_current()`. However, the protection is opt-in: code that uses bare `unsafe { &mut *ptr }` instead of `scoped_mut(ptr, &mut tok)` bypasses the guardrail entirely. The token only protects functions that participate in the protocol.

**Current state:** All four blocking IPC paths (ipc_send, ipc_receive, ipc_call, notification_wait) use the token protocol. Bare `&mut *ptr` casts in `endpoint.rs` (lines 103, 188, 207) and `notification.rs` (line 87) sit on fast paths that return without ever calling `block_current()` — the comments explain why. No machine enforcement yet.

**Fix:** Enforce via review and grep: `grep -n '&mut \*' src/ipc/` should return only `scoped_mut` calls in blocking functions. If a new blocking IPC function is added, it must use BlockToken. Consider an xtask lint that checks for bare `&mut *raw_ptr` in IPC files.

---

## SYS_RECV_NB naming inconsistency

**Where:** `lockjaw-types/src/syscall.rs:91`, `src/syscall/handler.rs:61`, `user/lockjaw-userlib/src/syscall.rs:271`

**What:** The syscall is named `SYS_RECV_NB` / `sys_recv_nb` but every other IPC syscall spells out "receive" (`SYS_RECEIVE`, `sys_receive`). Inconsistent abbreviation.

**Fix:** Rename to `SYS_RECEIVE_NB` / `sys_receive_nb` across all three files.

---

## lockjaw-userlib minor cleanup

**Where:** `user/lockjaw-userlib/`

**What:** Initial extraction left several minor issues. Status updated:

- ~~`ProcessMapping` is defined in both userlib and the kernel~~ — **resolved**: `user/lockjaw-userlib/src/process.rs:2` re-exports from `lockjaw-types::process::ProcessMapping`.
- No module-level doc comment on `lib.rs` — still missing.
- Inconsistent re-export strategy: `syscall::*` and `print::*` are glob-exported, `lockjaw_types::elf` is namespaced. Should be a documented choice.
- Empty `[lib]` section in `Cargo.toml` (line 11) — still present, no effect, just noise.

**Why:** Low-priority cleanup. None of these cause bugs.

**Fix:** Add doc comment. Standardize re-exports. Drop the empty `[lib]` section.

---

## Display DDI mode index race with hotplug displays

**Where:** `lockjaw-types/src/display.rs` (DisplayRequest::SetMode, GetMode)

**What:** The DDI identifies modes by list index. If the display is hotpluggable (e.g., HDMI), the mode list can change between `list_modes`/`get_mode` and `set_mode`. A client could request mode index 2 while the list has shifted, setting the wrong resolution.

**Why:** QEMU ramfb has a static mode list. No hotplug, no race.

**Fix:** Stable mode IDs (e.g., hash of width+height+refresh+format), or a generation counter the client passes with `set_mode` so the driver can reject stale requests.

---

## sys_close_handle ownership semantics

**Where:** `src/syscall/handler.rs` (sys_close_handle), all userspace programs

**What:** `sys_close_handle(handle)` reclaims the handle table slot and, for PageSet handles, decrements refcount via the revocation path (`free-on-zero` semantics). For non-PageSet handles (endpoint, notification, reply), the slot is freed but the underlying object is not — by design, since these are donated pages owned by the consuming process and never refcounted. The original "no backing memory freed" framing conflated these two cases.

**Current state:** PageSet handles correctly free pages on last-close (handle revocation walks all processes' handle tables before freeing). Non-PageSet objects remain effectively immortal — their `refcount` field on `ObjectHeader` exists but is not wired into any lifecycle for endpoint/notification/reply types.

**Fix:** Decide whether endpoints/notifications/replies should be refcounted (matches the "everything is a handle" microkernel ideal) or remain donation-owned (matches the current "you donated the page, you decide when to reclaim" model). If refcounted, extend the close path to drop them on last-close; if donation-owned, document that explicitly somewhere user-visible.

---

## Audit: drop guards for resource cleanup

**Where:** `src/process.rs` (create_process), `src/syscall/handler.rs` (sys_create_thread), and any kernel path that allocates multiple resources and rolls back manually on failure.

**What:** The `HeaderPageGuard` pattern in `src/cap/pageset_table.rs` is the model: RAII guards that free resources on drop unless explicitly taken. `create_process` (`src/process.rs:18-46`) now uses `PageGuard` and `Ttbr0Guard` throughout. But other allocation paths still use manual rollback — most visibly `sys_create_thread` in `src/syscall/handler.rs` (around line 757) where TCB and stack pages are deallocated by hand on error.

**Fix:** Continue applying the guard pattern to remaining manual-rollback paths. Each new fallible allocation chain should reach for guards by default.

---

## DTB-driven baud rate computation

**Where:** `src/arch/aarch64/uart.rs` (init_baud)

**What:** `init_baud()` hardcodes IBRD=26 / FBRD=3 for a 48 MHz UARTCLK (Pi 4B and QEMU). Future platforms with a different UARTCLK will produce the wrong baud rate. The proper fix is to read `clock-frequency` from the DTB's UART node and compute the divisors dynamically.

**Why bootstrap:** The Pi 4B DTB does not expose `clock-frequency` directly on the UART node — it uses phandle references into the VideoCore clock controller, which would require a full clock framework to resolve. Both currently supported platforms (QEMU virt, Pi 4B) use 48 MHz UARTCLK.

**Fix:** Implement a minimal clock resolution pass: follow the UART node's `clocks` phandle to its parent clock node and read `clock-frequency`. Compute IBRD/FBRD from the discovered clock rate. Until then, init_baud is called unconditionally with fixed 48 MHz divisors.

---

## Audit: push kernel state into lockjaw-types

**Where:** Kernel-side modules in `src/` that contain pure state machines, data structures, or decision logic with no hardware or `unsafe` dependencies.

**What:** Substantial progress: lockjaw-types has grown to ~16 K LOC across 25+ modules covering IPC state machine, scheduler model, process lifecycle/transfer/teardown, PageSet table + variable-size header, POSIX dispatch + VA layout, FAT32, FDT, and ELF loader. Kernel-side `src/cap` + `src/ipc` + `src/sched` together still hold ~3300 LOC. `docs/extraction-roadmap.md` lists the remaining priority targets.

**Outstanding push-shaped kernel code:**
- `create_process` outer orchestration (Priority 1 in extraction-roadmap)
- `sys_map_pages` VA decision (Priority 1)
- PageSet alloc rollback (Priority 1)
- Endpoint and notification runtime handlers (~560 LOC combined)
- Scheduler context-switch integration (~730 LOC)
- Revocation walks (~260 LOC) — recently added; could likely have parts extracted

**Fix:** Continue the push→pull conversion per `docs/extraction-roadmap.md`. Each new feature should land its decision logic in lockjaw-types first; the kernel mechanically executes.
