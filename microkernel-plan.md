# Rust Microkernel — ARM64 (AArch64) Development Plan

A capability-based microkernel inspired by seL4 and Zircon, written in Rust, targeting ARMv8-A (AArch64). All development and testing on QEMU `virt` machine.

## Architecture Decisions (Locked In)

- **Target:** AArch64 (ARMv8-A), QEMU `virt` machine, GICv3 interrupt controller
- **Language:** Rust (`#![no_std]`, `#![no_main]`), minimal inline assembly
- **Security model:** Handle-based access control (Lockjaw model — see docs/object-model.md)
- **IPC:** Synchronous endpoint-based (fast path register transfer)
- **Drivers:** All in userspace except interrupt controller (GIC) and timer
- **Memory:** User-allocated pages via PageSets — no in-kernel dynamic allocation
- **Toolchain:** `aarch64-unknown-none` target, `cargo` + linker script, QEMU for testing

## Prerequisites

```
rustup target add aarch64-unknown-none
rustup toolchain install nightly       # needed for -Z emit-stack-sizes
brew install qemu        # or apt install qemu-system-aarch64
cargo install cargo-binutils
cargo install cargo-call-stack
rustup component add llvm-tools
```

---

## Phase 1 — Boot to UART

Goal: See "Hello from kernel" printed in QEMU terminal.

### Milestone 1.1: Bare-metal binary that builds
- `cargo new --lib microkernel`, set up `#![no_std]`, `#![no_main]`
- `.cargo/config.toml` targeting `aarch64-unknown-none`
- Linker script (`linker.ld`) placing kernel at `0x4008_0000` (QEMU virt default)
- `_start` entry point in assembly: set stack pointer, zero BSS, jump to `kmain`
- **Verify:** `cargo build` succeeds, `objdump` shows expected sections and entry point

### Milestone 1.2: UART output
- PL011 UART at MMIO address `0x0900_0000` (QEMU virt)
- Minimal `uart_putc` / `uart_puts` via volatile writes
- `kmain()` prints "Hello from kernel" and loops
- **Verify:** `qemu-system-aarch64 -machine virt -cpu cortex-a53 -nographic -kernel kernel.bin` shows output

### Milestone 1.3: Panic handler with output
- `#[panic_handler]` that prints file/line to UART, then halts (`wfi` loop)
- Add `panic!("test")` call, confirm it prints location and halts
- **Verify:** Intentional panic shows formatted message in QEMU

### Milestone 1.4: Formatted printing
- Implement `core::fmt::Write` for UART
- `kprintln!` macro wrapping `write!`
- Print boot banner with kernel version, memory layout from linker symbols
- **Verify:** Structured boot log appears in QEMU

### Milestone 1.5: Stack depth verification (CI gate)
This is a build-time gate that runs on every build from this point forward. It proves two properties: no recursion exists in the kernel call graph, and worst-case stack depth fits within budget.

- Require Rust nightly for `-Z emit-stack-sizes`
- Add `cargo-call-stack` to the toolchain
- Create `xtask check-stack` (or Makefile target) that runs `cargo call-stack --target aarch64-unknown-none` on the kernel binary
- Assert: **zero cycles** in call graph (recursion absence proof)
- Assert: worst-case depth from `_start` < **3072 bytes** (normal path budget)
- Assert: worst-case depth from exception vector entries < **1024 bytes** (interrupt budget)
- Combined budget: 3072 + 1024 = 4096 (one page stack, these are proven worst-case so no extra headroom needed)
- **Verify:** Intentionally add a recursive function, confirm the tool flags the cycle. Add a 2KB stack-allocated array, confirm the depth exceeds budget. Remove both, confirm clean pass.

**Design rules enforced from this point forward (to keep the call graph statically analyzable):**
- No `dyn Trait` in kernel code — use generics or concrete types
- No function pointer tables — syscall dispatch uses `match`, not `fn` arrays
- No recursive data structures or recursive traversal (handle table lookup must be iterative)
- `core::fmt` goes through a concrete UART type, not `&mut dyn Write` — may need a thin wrapper to inline the write path so the tool can trace through it
- Interrupt handlers must be minimal (set flag, return) — all printing/complex logic runs in the non-interrupt path

**Stack budget accounting:**
The true worst case is `max(normal paths) + max(interrupt paths)` since an interrupt can arrive at the deepest point of a normal call chain. The check must validate both independently and confirm their sum fits in the allocated stack.

---

## Phase 2 — Memory Management

Goal: Physical frame allocator + page tables + virtual memory enabled.

### Milestone 2.1: Physical memory map
- Parse QEMU DTB (device tree blob) or hardcode QEMU virt RAM range (`0x4000_0000` — `0x4800_0000` for 128MB)
- Define `PhysAddr` / `VirtAddr` newtypes (no raw `usize` outside this module)
- `PhysFrame` newtype representing an aligned 4KB physical page
- **Verify:** Boot log prints detected memory range and frame count

### Milestone 2.2: Bitmap frame allocator
- Bitmap allocator over physical frames (static array sized to max RAM)
- `alloc_frame() -> Option<PhysFrame>`, `dealloc_frame(PhysFrame)`
- Mark kernel image frames and DTB as reserved at boot
- **Verify:** Allocate 10 frames, print their addresses, dealloc, reallocate — same addresses returned

### Milestone 2.3: Page table structures
- AArch64 4-level page table types (`L0Table`, `L1Table`, `L2Table`, `L3Table`)
- `PageTableEntry` with bitfield accessors (valid, AF, AP, SH, output address)
- Memory attribute indirection register (MAIR_EL1) setup for normal/device memory
- **Verify:** Construct a page table entry, print raw bits, confirm flags match expected values

### Milestone 2.4: Identity-map kernel + enable MMU
- Build initial page tables: identity-map kernel text/data/stack, device MMIO region
- Write `TTBR0_EL1` / `TTBR1_EL1`, configure `TCR_EL1`, set `SCTLR_EL1.M`
- Kernel continues running after MMU enable (identity mapping makes this transparent)
- **Verify:** kprintln works before AND after MMU enable (UART still reachable)

### Milestone 2.5: Higher-half kernel mapping
- Remap kernel to upper VA range (e.g. `0xFFFF_0000_4008_0000`)
- TTBR1 for kernel space, TTBR0 for userspace (empty for now)
- Fixup all kernel pointers (stack, UART) to use high addresses
- **Verify:** Boot log prints new kernel virtual addresses, UART still works

### Milestone 2.6: Kernel stack guard page + canary
- Map one unmapped 4KB page immediately below the kernel stack — data abort on overflow instead of silent corruption
- Write a known canary value (`0xDEAD_BEEF_DEAD_BEEF`) at the stack bottom (first word above guard page)
- Fill remaining stack with `0xCCCC_CCCC_CCCC_CCCC` pattern for high-water-mark analysis
- Add `check_stack_canary()` call — will be wired into the scheduler in Phase 5
- **Verify:** Intentionally overflow the stack in QEMU (large local array), confirm data abort fires from the guard page. Restore, confirm canary is intact during normal boot.

---

## Phase 3 — Exceptions & Interrupts

Goal: Trap to kernel on exceptions, handle timer interrupts for preemption.

### Milestone 3.1: Exception vector table
- `VBAR_EL1` vector table (16 entries × 128 bytes each)
- Stub handlers that print exception type + ESR_EL1 + ELR_EL1 and halt
- **Verify:** Trigger a deliberate fault (e.g. access unmapped address), see formatted exception info

### Milestone 3.2: GICv3 interrupt controller
- Initialize GICv3 distributor + redistributor + CPU interface
- Route timer interrupt (PPI 30) to core 0
- EOI (end-of-interrupt) handling
- **Verify:** kprintln from GIC init showing distributor version, supported IRQ count

### Milestone 3.3: Timer interrupt
- Configure `CNTV_TVAL_EL0` and `CNTV_CTL_EL0` for virtual timer
- Timer IRQ handler increments a tick counter
- Rearm timer in handler for periodic ticks (e.g. 10ms)
- **Verify:** Boot log shows tick counter incrementing every 10ms

---

## Phase 4 — Kernel Object Model & Handles

Goal: Typed kernel objects created via PageSet donation, accessed through handles.

### Milestone 4.1: Kernel object types
- `ObjectType` enum (YAGNI: only types needed now — `HandleTable`, `PageSet`)
- Per-type create-info structs (Vulkan pattern: same struct for size query and creation)
- `ObjectHeader` with type tag + page count at the start of every object
- **Verify:** Query sizes, create objects in donated pages, print headers

### Milestone 4.2: Handle table
- `HandleEntry` = (object physical address, type, rights bitmask)
- `HandleTable` = array of `HandleEntry` stored in donated pages
- `handle_lookup(table_paddr, handle, required_rights) -> Result<HandleEntry>`
- Rights: `Read`, `Write`, `Grant` as bitflags
- **Verify:** Create a HandleTable, insert handles, look them up, verify rights checks pass/fail

### Milestone 4.3: PageSets & object creation flow
- `PageSet` = 1..N physical pages allocated from the kernel page bitmap
- `alloc_pages(count) -> PageSet` allocates from the bitmap
- `donate(pageset) -> PhysAddr` hands pages to kernel for object creation
- A PageSet is either donated (kernel object) or mapped (MappedPages, Phase 6) — never both
- **Verify:** Alloc pageset, donate, create HandleTable, insert/lookup/remove handles

---

## Phase 5 — Threads & Context Switching

Goal: Two kernel threads alternating via timer preemption.

### Milestone 5.1: Thread Control Block
- `TCB` struct: saved register file (31 GPRs + SP + PC + PSTATE), CSpace root, VSpace root, scheduling state
- `thread_create(tcb_cap, cspace_cap, vspace_cap, entry_point, stack_pointer)`
- **Verify:** Create a TCB, print its fields

### Milestone 5.2: Context switch
- `context_switch(old: &mut TCB, new: &TCB)` — save/restore all registers via asm
- Manually switch between two kernel-mode threads that each print a counter
- **Verify:** Interleaved "Thread A: 1", "Thread B: 1", "Thread A: 2", ... output

### Milestone 5.3: Scheduler
- Run queue (simple FIFO / round-robin linked list)
- Timer interrupt handler calls `schedule()` → picks next thread → context switches
- Wire `check_stack_canary()` into the scheduler — runs on every context switch, panics if corrupted
- Idle thread that executes `wfi` when queue is empty
- **Verify:** Three threads printing at different rates, all getting time slices. Stack canary intact across sustained scheduling.

---

## Phase 6 — Syscall Interface

Goal: Threads in EL0 (userspace exception level) making syscalls to EL1 kernel.

### Milestone 6.1: Drop to EL0
- Allocate a user page table (TTBR0), map a single code page + stack page
- Copy a trivial function into the user page
- `eret` to EL0 with user PC and SP set
- **Verify:** User function runs (it will fault on first syscall attempt — that's expected)

### Milestone 6.2: Syscall trap handler
- `SVC #0` handler in exception vector (Synchronous, lower EL, AArch64)
- Decode syscall number from `x8`, arguments from `x0-x5`
- Implement `sys_debug_putc(char)` — kernel prints char on behalf of user
- **Verify:** Userspace code calls `svc #0` with a character, it appears on UART

### Milestone 6.3: Core syscall set
- `sys_send(endpoint_cap, msg_regs)` — send on endpoint
- `sys_recv(endpoint_cap) -> msg_regs` — receive on endpoint
- `sys_call(endpoint_cap, msg_regs) -> msg_regs` — send + wait for reply
- `sys_reply(msg_regs)` — reply to caller
- `sys_yield()` — voluntary reschedule
- **Verify:** Syscall dispatch table works, invalid syscall numbers return error

---

## Phase 7 — IPC

Goal: Two userspace threads exchanging messages through an endpoint.

### Milestone 7.1: Endpoint object
- `Endpoint` kernel object: send queue + receive queue (lists of blocked TCBs)
- `send` on endpoint with no receiver → block sender, enqueue
- `recv` on endpoint with no sender → block receiver, enqueue
- **Verify:** Thread A sends, blocks. Thread B receives, both unblock. Print message contents.

### Milestone 7.2: Fast-path IPC
- When sender and receiver are both ready: transfer `x0-x3` directly between saved register contexts (no memory copy)
- Fast path check: endpoint has waiting receiver, message fits in registers, no faults
- **Verify:** Measure IPC round-trip in timer ticks — should be dramatically faster than slow path

### Milestone 7.3: Call/Reply pattern
- `sys_call` = send + block-for-reply (single syscall, one capability lookup)
- Receiver gets a one-time reply capability automatically
- `sys_reply` uses the reply cap, wakes caller
- **Verify:** Client/server pattern — client calls, server computes, replies. Print round-trip.

---

## Phase 8 — Userspace Processes

Goal: Multiple isolated address spaces running independent ELF binaries.

### Milestone 8.1: ELF loader
- Parse ELF64 header + program headers (PT_LOAD segments)
- Allocate frames, map into a new TTBR0 page table, copy segment data
- Set entry point from ELF header
- **Verify:** Load a minimal no_std Rust ELF, it runs in its own address space

### Milestone 8.2: Init process
- Kernel boots, creates init process from embedded ELF blob
- Init receives: PageSet handles for all remaining free pages, endpoint handles for IPC to kernel
- Init prints "Hello from userspace init" via debug syscall
- **Verify:** Init runs, prints, doesn't crash

### Milestone 8.3: Process spawning from init
- Init donates PageSets → creates new TCB + HandleTable + VSpace + MappedPages
- Loads a second ELF binary, sets up its address space, starts it
- Two independent processes running
- **Verify:** Init and child both printing, isolated address spaces confirmed (child can't read init memory)

---

## Phase 9 — Device Drivers in Userspace

Goal: UART driver as a userspace process, kernel no longer directly accesses UART after boot.

### Milestone 9.1: Device memory mapping syscall
- `sys_map_device_memory(frame_cap, vaddr, size)` maps MMIO pages into caller's address space with device memory attributes (nGnRE)
- **Verify:** Userspace process maps UART MMIO range, writes directly to it, characters appear

### Milestone 9.2: IRQ notification
- `Notification` kernel object (semaphore-like, capability-gated)
- `sys_irq_bind(irq_cap, notification_cap)` routes hardware IRQ to a notification
- `sys_wait(notification_cap)` blocks until signaled
- **Verify:** UART RX interrupt wakes userspace driver, driver reads character

### Milestone 9.3: UART server
- Standalone userspace UART driver process
- Other processes send "print this string" IPC messages to it
- Kernel UART access removed (except early boot before driver starts)
- **Verify:** Init process sends strings to UART server, output appears on QEMU console

---

## Phase 10 — POSIX Compatibility (Stretch Goal)

Goal: Run a simple POSIX binary (e.g. busybox echo) on the microkernel.

### Milestone 10.1: POSIX personality server skeleton
- Userspace server process that accepts IPC messages representing POSIX syscalls
- Maintains per-client state: file descriptor table, PID
- Responds to `write(1, buf, len)` by forwarding to UART server
- **Verify:** POSIX client calls `write(STDOUT_FILENO, "hello", 5)`, output appears

### Milestone 10.2: musl libc port
- Fork musl, replace `__syscall` backend with IPC to POSIX server
- Build musl as `aarch64-linux-musl` cross-target with custom syscall layer
- Link a trivial C program: `int main() { puts("hello"); return 0; }`
- **Verify:** C binary runs on microkernel, prints via POSIX server → UART server chain

### Milestone 10.3: Filesystem (tmpfs)
- Tmpfs server implementing `open`, `read`, `write`, `close`, `stat` on in-memory files
- POSIX server routes `open("/tmp/...")` to tmpfs server
- **Verify:** C program creates a file, writes to it, reads it back, contents match

---

## Key Design Constraints

- **No `alloc` in kernel.** All kernel object memory comes from user-donated PageSets. The kernel has a fixed-size boot region (stack, boot page tables, page allocator bitmap) and that's it. `core::fmt` scratch space lives on the stack.
- **Proven stack safety.** `cargo-call-stack` runs on every build (from Milestone 1.5). Build fails on any call graph cycle (recursion) or if worst-case depth exceeds budget. Normal path ≤ 3072 bytes, interrupt path ≤ 1024 bytes, total ≤ 4096 (one page). Guard page + canary provide runtime backup.
- **Statically analyzable call graph.** No `dyn Trait`, no function pointer tables, no recursion in kernel code. Syscall dispatch uses `match`. `core::fmt` writes through a concrete type, not `&mut dyn Write`. These rules exist to keep `cargo-call-stack` analysis precise.
- **Minimal syscall count.** Target ~10 syscalls (Send, Recv, Call, Reply, Yield, AllocPages, CreateObject, Map, IRQBind, DebugPutc). Resist adding more.
- **`unsafe` containment.** All unsafe code lives in arch/, mm/, and context switch modules. Everything above the HAL (capability system, scheduler, IPC logic) is safe Rust.
- **One architecture.** AArch64 only. Don't abstract over architectures until Phase 8+. YAGNI until then.
- **Test in QEMU first, always.** No real hardware until the full IPC path works.

## File Structure (Initial)

```
microkernel/
├── .cargo/config.toml          # target = aarch64-unknown-none
├── Cargo.toml
├── Makefile                    # build, run-qemu, check-stack targets
├── linker.ld
├── xtask/                      # build tooling (cargo xtask pattern)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs             # check-stack: runs cargo-call-stack, enforces budgets
├── src/
│   ├── main.rs                 # kmain + panic handler
│   ├── arch/
│   │   └── aarch64/
│   │       ├── boot.S          # _start, stack setup, BSS clear
│   │       ├── exceptions.S    # vector table stubs
│   │       ├── context.S       # context switch asm
│   │       ├── mmu.rs          # page table manipulation
│   │       └── gic.rs          # GICv3 driver
│   ├── mm/
│   │   ├── page_alloc.rs       # physical page allocator (bitmap)
│   │   ├── page_table.rs       # page table types + mapping API
│   │   ├── addr.rs             # PhysAddr / PhysPage newtypes
│   │   └── stack.rs            # guard page setup, canary write/check
│   ├── cap/
│   │   ├── object.rs           # ObjectType, create-info structs, query/create
│   │   ├── handle_table.rs     # HandleEntry + handle insert/lookup/remove
│   │   ├── rights.rs           # rights bitmask
│   │   └── pageset.rs          # PageSet allocation and donation
│   ├── sched/
│   │   ├── tcb.rs              # thread control block
│   │   └── scheduler.rs        # run queue + scheduling policy
│   ├── ipc/
│   │   └── endpoint.rs         # endpoint object + send/recv
│   └── syscall/
│       └── handler.rs          # syscall dispatch (match, no fn pointers)
└── user/                       # userspace binaries (separate crate)
    ├── init/
    └── uart-driver/
```

## Reference Material

- seL4 Reference Manual (especially Ch. 2: Kernel Objects, Ch. 4: IPC)
- Zircon Kernel Concepts (Fuchsia docs)
- `rust-embedded/aarch64-cpu` crate for register definitions
- QEMU `virt` machine source (`hw/arm/virt.c`) for memory map
- ARM Architecture Reference Manual (ARMv8-A) for exception levels, page table format, system registers
