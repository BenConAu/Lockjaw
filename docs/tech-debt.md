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

**Where:** Not yet implemented (planned for Milestone 9.3)

**What:** IRQ-to-Notification bindings stored in a static array of 32 entries. Cannot grow dynamically.

**Why bootstrap:** Same pattern as PageSet table. A single UART IRQ binding doesn't need more.

**Fix:** Device manager process owns the binding table in allocated pages, receives registration requests from drivers via IPC, calls sys_bind_irq on their behalf. Policy (which driver gets which IRQ, conflict detection) moves to userspace. The kernel syscall stays simple: one binding at a time.

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

## Duplicated ELF parser (kernel + init)

**Where:** `lockjaw-types/src/elf.rs` and `user/init/src/main.rs`

**What:** Init has its own minimal ELF parser (~80 lines of byte parsing). The kernel has the same parser in lockjaw-types. They are functionally identical but not shared — init is a separate Cargo project that cannot depend on lockjaw-types.

**Why:** Init is not a workspace member (different linker script, different build config). Adding lockjaw-types as a path dependency from init would require careful workspace configuration to avoid conflicting targets.

**Fix:** Create a `lockjaw-userlib` no_std crate that both init and other userspace processes depend on. It would contain the ELF parser, syscall wrappers, and common types. Alternatively, make lockjaw-types publishable and add it as a dependency to each user crate.

---

## ipc_msg field used as scratch in TCB

**Where:** `src/process.rs` (line 129-130)

**What:** New processes store their ELF entry point and user stack top in the TCB's `ipc_msg[0]` and `ipc_msg[1]` fields. These are IPC message registers repurposed as temporary storage because the thread hasn't started IPC yet.

**Why:** Adding dedicated fields to the TCB would increase its size. The ipc_msg fields are unused until the thread starts IPC operations.

**Fix:** Add `user_entry_point: u64` and `user_stack_top: u64` fields to the TCB struct. The ipc_msg field should only be used for IPC.
