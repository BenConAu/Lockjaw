# Technical Debt Log

Known limitations introduced for bootstrapping. Each item documents what we did, why, and what the eventual fix is. Unlike the YAGNI parking lot (removed code to add later), this tracks code that exists but is limited.

---

## Static PageSet table (32-slot cap)

**Where:** `src/cap/pageset_table.rs`

**What:** PageSets are tracked in a fixed-size static array of 32 slots. No per-process tracking, no cleanup on process death.

**Why bootstrap:** The eventual design (PageSets as kernel objects in per-process handle tables) has a circularity — you need pages to create the object that tracks pages. Bootstrapping requires pre-created PageSets for init.

**Fix:** Device manager process owns the table in its own allocated pages. Kernel pre-creates bootstrap PageSets for init at boot. Per-process accounting via handle tables.

---

## Static IRQ binding table (32-slot cap)

**Where:** `src/arch/aarch64/irq_bind.rs`

**What:** IRQ-to-Notification bindings stored in a static array of 32 entries. Cannot grow dynamically. No unbind, no conflict detection, no ownership tracking.

**Why bootstrap:** Same pattern as PageSet table. A single UART IRQ binding doesn't need more.

**Fix:** Superseded by the device manager (see below). The device manager owns the binding table, the kernel syscall stays simple: one binding at a time.

---

## Kernel identity map in user TTBR0

**Where:** `src/arch/aarch64/vmem.rs` (create_address_space, L1[1] and L2[4])

**What:** Every user address space includes the kernel's physical address range (RAM at L1[1], device MMIO at L2[4]) with AP_RW_EL1. This is because the kernel binary is linked at physical addresses and VBAR_EL1 points to a physical address.

**Why bootstrap:** Relinking the kernel at higher-half VAs requires changing the linker script origin, adding a boot trampoline that runs at physical addresses before jumping to higher-half, and updating every function/static address. Significant change.

**Fix:** Relink the kernel at higher-half VAs (linker.ld ORIGIN = 0xFFFF_0000_4008_0000). Add a boot trampoline in boot.rs that identity-maps initially, then jumps to higher-half after TTBR1 is installed. After that, TTBR0 can be pure user pages with no kernel entries.

---

## Syscall handler code repetition

**Where:** `src/syscall/handler.rs`

**What:** The `lookup_handle` helper was extracted after noticing the same 10-line pattern copied across 8 syscall handlers. Other patterns may still be repeated — a systematic scrub has not been done.

**Why:** Each syscall was added incrementally, one per milestone. Code was written for the immediate need without looking back at what could be shared.

**Fix:** Periodically scrub the syscall handler for repeated patterns and extract helpers. The lookup_handle refactor is the model for how to do this.

---

## MAX_THREADS = 8 static scheduler array

**Where:** `src/sched/scheduler.rs`

**What:** The run queue is a fixed-size array of 8 slots. Cannot grow. add_thread returns false when full.

**Why bootstrap:** A dynamic run queue requires either kernel allocation (violates our principle) or a user-donated page for the queue. For 3 processes (init, hello, UART driver) plus 2 IPC threads + idle, 8 is enough.

**Fix:** User-donated page for the run queue, similar to how handle tables work. The scheduler would be initialized with a donated page that holds the thread array.

---

## UnsafeCell globals assume single-core

**Where:** `src/cap/pageset_table.rs`, `src/arch/aarch64/irq_bind.rs`

**What:** Kernel globals use `UnsafeCell` with a manual `unsafe impl Sync` to avoid `static mut` UB warnings. The safety argument is "single-core kernel, no concurrent access during a syscall." This is true today but breaks under SMP where multiple cores can execute syscalls concurrently.

**Why bootstrap:** Proper locking (spinlocks, per-CPU data) requires an SMP-aware synchronization primitive that doesn't exist yet. Single-core QEMU virt doesn't need it.

**Fix:** When adding SMP support, replace bare `UnsafeCell` wrappers with a kernel spinlock type (e.g. `SpinMutex<T>` that disables IRQs on lock). Audit every `unsafe impl Sync` for the same pattern. The IRQ binding table (`irq_bind.rs`) still uses `static mut` and needs the same treatment.

---

## No safe page abstraction in userspace

**Where:** `user/lockjaw-userlib/src/lib.rs` (`zero_page_at_va`)

**What:** The kernel's `zero_page(PhysAddr)` is safe because `PhysAddr` is a trusted type from the page allocator. The userspace equivalent `zero_page_at_va(u64)` is unsafe because user virtual addresses are raw `u64` values with no type-level proof that they're mapped.

**Why bootstrap:** Userspace has no validated address types. Adding a `MappedPage` type returned by `sys_map_pages` would make page operations safe, but requires designing the userspace memory management API.

**Fix:** Introduce a `MappedPage` type in lockjaw-userlib that wraps a VA returned by a successful `sys_map_pages`. `zero_page_at_va` and other page operations would take `MappedPage` instead of raw `u64`, making them safe.

---

## No device manager process

**Where:** Affects `src/arch/aarch64/irq_bind.rs`, `src/syscall/handler.rs` (sys_map_pages with MAP_FLAG_DEVICE, sys_bind_irq)

**What:** Drivers currently call sys_bind_irq and sys_map_pages(MAP_FLAG_DEVICE) directly with hardcoded physical addresses and INTIDs. There is no authority controlling which driver gets which hardware resources. Any process that can call these syscalls can map any MMIO region or claim any IRQ.

The eventual design is a **device manager** process that:

1. **Parses the DTB (device tree blob)** at boot to discover hardware: MMIO base addresses, sizes, IRQ numbers, compatible strings. QEMU `-machine virt` provides a DTB at a known address that the kernel can pass to the device manager.

2. **Owns the IRQ binding table** in its own allocated pages (replacing the static 32-slot kernel table). Drivers request IRQ bindings via IPC to the device manager, which calls sys_bind_irq on their behalf.

3. **Grants MMIO access** by allocating PageSets covering device memory regions and donating them to the requesting driver. The device manager is the only process that calls sys_map_pages with MAP_FLAG_DEVICE — drivers receive pre-mapped pages or capabilities to map them.

4. **Enforces policy:** which driver gets which device (by compatible string or explicit config), conflict detection (two drivers claiming the same MMIO range or IRQ), and revocation.

**Why bootstrap:** Phase 9 only needs one UART driver with one known MMIO address and one IRQ. Hardcoding works. The DTB parser and resource arbitration are substantial — worth their own phase.

**Fix:** Dedicated phase. The kernel's sys_bind_irq and sys_map_pages syscalls stay as-is (simple, low-level primitives). The device manager sits on top as the userspace authority that decides who gets to call them and with what arguments.

---

## Single-handle copy at process creation

**Where:** `src/process.rs`, `src/syscall/handler.rs` (sys_create_process x5 parameter)

**What:** sys_create_process copies at most one handle from the parent's handle table into the child's. x5 = handle index to copy, or u64::MAX for none. This is the only mechanism for capability transfer between processes.

**Why bootstrap:** The UART driver needs exactly one handle (the IPC endpoint from init). One handle is enough for Phase 9.

**Fix:** Either extend to an array of handles (x5 = pointer to handle index array, x6 = count), or implement IPC-based capability transfer where handles can be sent in IPC messages (like seL4's CNode mint/copy operations). The latter is the proper microkernel approach — capabilities flow through IPC, not just at creation time.

---

## block_current returns immediately when no thread is Ready

**Where:** `src/sched/scheduler.rs:84`, `lockjaw-types/src/scheduler.rs:25`

**What:** `block_current()` marks the thread Blocked and calls `schedule()`. But if no other thread is Ready, the scheduler model returns `StayOnCurrent`, and `schedule()` flips the thread back to Running and returns. Callers like `notification_wait` and IPC `execute_ipc` assume `block_current()` does not return until the thread is unblocked — they resume and read state that hasn't been updated yet.

**Why bootstrap:** With the current workload, there is always at least one Ready thread (the boot/init thread or the IPC test threads). The all-blocked case hasn't been triggered.

**Fix:** `block_current()` must spin or wfi in a loop until the thread is actually unblocked. The scheduler should idle (wfi) when all threads are blocked instead of returning to the caller. Alternatively, keep an explicit idle thread that is always Ready.

---

## Kernel threads leave stale user TTBR0 in hardware

**Where:** `src/sched/scheduler.rs:147`, `src/main.rs:280-310`

**What:** Kernel threads are created with `ttbr0_paddr = 0`. The scheduler only writes TTBR0_EL1 when `new_ttbr0 != 0`. When switching from a user process to a kernel thread, the previous user process's page table stays in TTBR0_EL1. An accidental lower-half access from a kernel thread would hit the previous process's memory instead of faulting cleanly.

**Why bootstrap:** Kernel threads only access higher-half addresses (via TTBR1). No kernel code accidentally touches the lower half. The risk is latent.

**Fix:** When switching to a thread with `ttbr0_paddr == 0`, zero out TTBR0_EL1 (or set it to an empty page table) so lower-half accesses fault cleanly. This prevents any future kernel bug from silently accessing user memory.

---

## SYS_RECV_NB naming inconsistency

**Where:** `lockjaw-types/src/syscall.rs`, `src/syscall/handler.rs`, `user/lockjaw-userlib/src/syscall.rs`

**What:** The syscall is named `SYS_RECV_NB` / `sys_recv_nb` but every other IPC syscall spells out "receive" (`SYS_RECEIVE`, `sys_receive`). Inconsistent abbreviation.

**Fix:** Rename to `SYS_RECEIVE_NB` / `sys_receive_nb` across all three files.

---

## lockjaw-userlib minor cleanup

**Where:** `user/lockjaw-userlib/`

**What:** Several minor issues from the initial extraction:
- `ProcessMapping` is defined in both userlib and the kernel (`src/process.rs`). Should live in lockjaw-types so both sides share one definition.
- No module-level doc comment on `lib.rs`.
- Inconsistent re-export strategy: syscall/print are glob-exported, elf is namespaced. Should be a documented choice.
- Empty `[lib]` section in Cargo.toml (no effect, just noise).

**Why:** Low-priority cleanup. None of these cause bugs.

**Fix:** Move `ProcessMapping` to lockjaw-types. Add doc comment. Clean up Cargo.toml. Standardize re-exports.
