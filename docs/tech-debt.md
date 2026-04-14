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

## ipc_msg field used as scratch in TCB

**Where:** `src/process.rs` (line 129-130)

**What:** New processes store their ELF entry point and user stack top in the TCB's `ipc_msg[0]` and `ipc_msg[1]` fields. These are IPC message registers repurposed as temporary storage because the thread hasn't started IPC yet.

**Why:** Adding dedicated fields to the TCB would increase its size. The ipc_msg fields are unused until the thread starts IPC operations.

**Fix:** Add `user_entry_point: u64` and `user_stack_top: u64` fields to the TCB struct. The ipc_msg field should only be used for IPC.

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

## Polling UART server

**Where:** `user/uart-driver/src/main.rs`

**What:** The UART driver polls both RX (via UARTFR MMIO read) and TX (via sys_recv_nb) in a loop with sys_yield between iterations. It burns a scheduler slot every tick even when idle.

**Why bootstrap:** The proper solution is bound notifications (seL4-style multiplexed wait): bind a notification to a thread's TCB so that sys_receive wakes on either an IPC message or a notification signal. This lets the driver sleep until work arrives. Implementing bound notifications requires modifying the IPC receive path, the notification signal path, and the TCB — substantial kernel work.

**Fix:** Add bound notifications. TCB gets a `bound_notif_paddr` field. sys_receive checks the bound notification before blocking — if it fired, return immediately with a flag. notification_signal checks if the bound TCB is blocked on an endpoint and wakes it. The driver loop becomes: `sys_receive(endpoint)` → if notification: handle RX; if IPC: handle TX. Zero CPU when idle.

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
