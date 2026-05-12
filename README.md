# Lockjaw

A capability-based microkernel written in Rust, targeting AArch64 (ARMv8-A). Runs on QEMU `virt` machine **and** boots end-to-end on a real Raspberry Pi 4B from the same binary — DTB-driven platform discovery, GICv2 + spin-table SMP, kernel image linked at a fixed higher-half VA with the load physical address discovered at runtime. Inspired by seL4 and Zircon, but with its own object model.

## What is this?

Lockjaw is a from-scratch microkernel that explores a middle ground between seL4's rigorous user-controlled memory model and Zircon's pragmatic handle-based API. The kernel never dynamically allocates memory. Userspace requests physical pages, then either maps them for its own use or donates them to the kernel to create objects like threads, IPC endpoints, and handle tables.

The design follows a few core principles:

- **Kernel never allocates.** All object memory comes from user-donated pages (PageSets). The kernel has only a fixed-size boot region in BSS.
- **Handle-based access control.** Every kernel object is accessed through an integer handle with an associated rights bitmask. No handle, no access.
- **Vulkan-inspired create-info pattern.** Each object type has its own create-info struct used for both size queries and creation. Same struct, no mismatch.
- **Proven stack safety.** A custom build tool analyzes the call graph and per-function stack sizes from four entry points (_start, _secondary_start, __vec_sync_lower, __vec_irq) on every build. Indirect calls must be annotated or the build fails.
- **Map or donate, never both.** A PageSet is consumed when donated for a kernel object. Consume is a transactional two-phase operation (validate + apply) that walks every live process's handle table, clears stale cross-process exported handles, and frees the header — no tombstones, no leaks.
- **Verified IPC state machine.** The IPC endpoint logic is driven by a pure state machine model that is exhaustively explored at test time -- all reachable states, all transitions, all effect orderings verified. Kernel IPC handlers match on typed decision enums (SendDecision, ReceiveDecision, CallDecision, ReplyDecision) returned by lockjaw-types. No inline state branching in kernel code.
- **Pull over push.** Kernel code is organized by integration shape: pull (types drives sequencing), plan/apply (types returns a decision, kernel executes), or push (kernel calls helpers). Push is treated as highest review-risk; the extraction rubric converts push to pull wherever possible.
- **All MMIO through the device manager.** Drivers cannot map arbitrary physical addresses. The device manager discovers hardware from the DTB and issues tracked PageSets for MMIO pages. Only processes that receive an MMIO PageSet can map device memory.
- **Unforgeable caller identity.** IPC endpoints carry kernel-assigned opaque caller tokens. When a handle is exported, the kernel assigns a monotonic per-endpoint token stored in the handle entry. Servers query the token after receive to scope resources per-client. Tokens identify handle lineage, not processes — delegates inherit the original token. Token 0 is receive-only; send/call with token 0 is rejected by the kernel.
- **Decoupled link VA from load PA.** The kernel image is linked at a fixed higher-half VA in its own L0[1] region (`0xFFFF_0080_0000_0000`), independent of the physical address firmware loads it at. The boot trampoline discovers the actual load PA via PC-relative (`adr _start`), computes a per-boot phys offset, and maps the runtime PA range at the linker VA via 4 KB L3 PTEs. Same binary boots on QEMU (load PA `0x40200000`) and Pi 4B (load PA `0x80000`); neither linker ORIGIN nor any code path needs adjustment. Userspace TTBR0 carries no kernel entries — no kernel identity, no device MMIO. The only PA-aware input is the DTB.
- **Typed VA regimes.** Three sibling newtypes — `PhysAddr` for physical memory, `KernelVa` for the KVM allocator pool (`0xFFFF_8000_0000_0000`, where typed kernel objects live), `KernelImageVa` for the kernel image region (`0xFFFF_0080_0000_0000`). `compile_fail` doctests prove they cannot be assigned across regimes. Per-thread kernel stack base is a typed `KernelStackBase::{Image(KernelImageVa), Pool(KernelVa)}` enum so `finish_exit`'s free-path choice is `match`-driven and the wrong path is unrepresentable.

## What works today

Lockjaw boots on QEMU with up to 4 cores (`-smp 4`), manages virtual memory with a buddy allocator supporting contiguous DMA allocation, handles interrupts, runs preemptively scheduled threads across multiple CPUs with a Giant Kernel Lock, serves 28 syscalls from EL0 userspace, passes messages between threads via synchronous IPC with Reply objects and kernel-assigned caller tokens for multi-client isolation, runs ten isolated userspace processes loaded from ELF binaries (init, hello, device-manager, uart-driver, ramfb-driver, virtio-blk-driver, fat32-server, fat32-test, posix-server, plus a musl-built `hello, lockjaw` test client spawned by the personality server), has a device manager that discovers hardware from the DTB with probe and claim-by-address protocols, a UART driver, a ramfb display driver, a VirtIO block driver that reads from a virtual disk via virtqueues, a FAT32 filesystem server that mounts the disk and serves open/read/close over IPC, and a POSIX personality server that runs statically-linked patched-musl binaries — including reading `/HELLO.TXT` via `fopen + fread + fclose` and allocating an 8 MiB buffer through `malloc` (musl stdio + mmap + cross-process file I/O all working end-to-end).

```
=== Lockjaw Microkernel v0.1.0 ===
Physical memory: 0x40000000 - 0x48000000 (32768 pages)
  Page allocator: 642 reserved, 32126 free
MMU enabled
Higher-half active
GIC initialized, timer PPI 27 enabled
Scheduler started.
Loading init process...
Dropping to EL0...
Hello from userspace init!
init: hello spawned OK
init: device-manager spawned OK
init: uart-driver spawned OK
init: ramfb-driver spawned OK
init: blk-driver spawned OK
devmgr: parsed DTB, 49 devices
uart-driver: claimed PL011
uart-driver: server ready
ramfb: claimed fw_cfg
ramfb: display configured
blk: found virtio-blk device            # with -drive flag
blk: selftest read OK, sector 0 = [4c 4f 43 4b ...]
blk: serving
fat32: mounted, cluster_size=512 bytes, root_cluster=2
posix-server: spawning posix-hello...
posix-server: posix-hello spawned OK
posix-server: POSIX_INIT OK
hello, lockjaw                          # ← from a real musl-built static binary
posix-hello: hello from fat32           # ← fopen + fread on /HELLO.TXT via FAT32 IPC
posix-hello: malloc 1MB ok              # ← musl malloc -> mmap -> server mmap_table
posix-hello: malloc 8MB ok              # ← single-PageSet 64 MiB-capable mmap
posix-server: child exit
[IPC BENCHMARK] 10000 call/reply round-trips in 74 ticks
[IPC BENCHMARK] 135 round-trips per tick
```

### Completed phases

**Phase 1 -- Boot to UART.** Bare-metal Rust binary boots on QEMU `virt`, prints to PL011 UART via MMIO, has a formatted `kprintln!` macro and a panic handler that prints file/line/message.

**Phase 2 -- Memory Management.** Buddy allocator over 128 MB of RAM (32,768 pages) with contiguous multi-page allocation for DMA buffers. AArch64 4-level page tables with identity mapping, then higher-half kernel mapping via TTBR1 (kernel at `0xFFFF_0000_xxxx_xxxx`). Unmapped guard page below the kernel stack with a canary value checked on every context switch.

**Phase 3 -- Exceptions and Interrupts.** Exception vector table with full register save/restore (31 GPRs + ELR/SPSR/ESR). GICv3 interrupt controller initialization. Virtual timer firing every 10ms for preemptive scheduling. Structured crash diagnostics: ESR decode, address classification, stack overflow detection, thread ID, syscall breadcrumb.

**Phase 4 -- Kernel Object Model.** Typed kernel objects created in user-donated pages via the Vulkan-style create-info pattern (query size, allocate PageSet, donate, create). Handle tables with insert/lookup/remove and rights checking (Read, Write, Grant). PageSets consumed on donation to prevent reuse.

**Phase 5 -- Threads and Context Switching.** Thread Control Blocks with per-thread stacks. Assembly `context_switch` saves/restores callee-saved registers (SavedContext struct with compile-time offset assertions against the assembly) and swaps SP. Round-robin scheduler driven by the timer interrupt. Preemptive multithreading verified with interleaved output from concurrent threads.

**Phase 6 -- Syscall Interface.** Userspace code runs at EL0 (unprivileged). SVC traps to kernel via separate lower-EL exception vector. Syscall dispatch on x8 register. Typed error returns: x0 = SyscallError (always), x1 = value, x1-x4 = IPC message words. User page tables in TTBR0 with PXN/UXN security bits.

**Phase 7 -- IPC.** Synchronous rendezvous message passing through Endpoint objects. Four message registers (x1-x4) transferred between threads. Send/receive with blocking, call/reply for client/server patterns using per-client Reply objects (eliminates multi-caller corruption). Non-blocking receive. Multiplexed wait (sys_wait_any) with threshold-based readiness. IPC state machine exhaustively verified: 89 reachable states (3-thread model), 8 invariants checked. The kernel's IPC is driven entirely by the verified model.

**Phase 8 -- Userspace Processes.** Per-process TTBR0 page tables swapped by the scheduler on context switch. ELF64 parser loads the init process from an embedded binary. Init runs at EL0 and spawns child processes entirely from userspace. Bootstrap channel protocol (Zircon-inspired): child calls handle 0, parent exports handles via sys_export_handle, replies with indices.

**Phase 9 -- Userspace Drivers.** UART driver runs entirely in userspace. Receives its server endpoint and device-manager endpoint via bootstrap. Event loop using sys_wait_any multiplexes IPC requests and hardware interrupts. Notification objects serve as timeline semaphores for IRQ delivery. Init prints messages through the UART driver via IPC.

**Phase 10 -- Device Manager and Display Driver.** Device manager process parses the Flattened Device Tree (DTB) at boot to discover hardware. Serves CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE (with explicit status codes: PROBE_OK/END/CLAIMED/ERR), and CMD_CLAIM_BY_ADDR (TOCTOU-safe claim by stable MMIO address) requests from drivers via IPC. Probe uses absolute indexing over all matching DTB nodes for stable concurrent enumeration. Creates tracked MMIO PageSets with sub-page offset support (multiple virtio-mmio devices share a 4K page). ramfb display driver claims fw_cfg from the device manager, allocates a contiguous DMA framebuffer, configures the display via the fw_cfg DMA protocol, and renders a test pattern.

**Phase 11 -- SMP.** Secondary CPUs booted via PSCI CPU_ON. Per-CPU stacks in the linker script (2MB-aligned, 4 guard+stack pairs). Per-CPU data via TPIDR_EL1 with narrow accessors. Giant Kernel Lock (ticket lock from lockjaw-types, host-testable with multi-threaded tests) serializes all kernel execution. Scheduler model adapted for per-CPU current threads. Exception handlers acquire/release GKL. Kernel threads run cooperatively under the GKL with IRQs masked. Idle threads release GKL and halt in wfi. Process entry releases GKL before eret to EL0. INTID 0 reserved for future cross-core reschedule SGI (parked until fine-grained locking).

**Phase 12 -- PageSet Lifecycle.** Mapping tracking, ownership transfer with ProcessTransferPlan (deduplication), refcounting with free-on-zero, process exit cleanup via ProcessTeardownPlan with construction-safe narrowing (separate step variants for with/without address space, making illegal unmap-during-teardown unrepresentable). Cross-process handle revocation: consume_pageset is now a transactional two-phase (validate + apply) operation that walks every live process's handle table, clears stale exported handles, decrements per-handle refcount/map_count, and frees the header — replacing the previous tombstone-leak pattern. sys_create_process restructured to push every fallible step into the validate phase (scheduler::has_room precheck, parent_handle_to_copy validation, consume_validate per header) so the apply phase cannot fail mid-stream. Variable-size PageSetHeader: 16-byte fixed metadata followed by an inline u64 array spanning multiple physically-contiguous header pages. Page-addr access is gated by a `BackedHeader<'a>` wrapper that carries trusted (count, backing_pages) witnesses from the global PageSetTable rather than from the on-disk header itself, so a corrupted header cannot silently truncate or extend operations. Lifts the previous 510-pages-per-set cap to a practical 64 MiB.

**Phase 13 -- Caller Tokens.** HandleEntry redesigned with typed HandleKind enum (repr(C, u8) with per-type metadata: caller_token on Endpoint, mapped_va_page on PageSet). Kernel assigns monotonic u64 tokens per endpoint on sys_export_handle and create_process handle copy. Token 0 = receive-only; send/call with token 0 is rejected. Servers query tokens via SYS_QUERY_CALLER_TOKEN (syscall 26). Tokens identify handle lineage for capability delegation. Integration test verifies nonzero token delivery.

**Phase 14 -- VirtIO Block Driver.** VirtIO MMIO transport with modern (non-legacy) device support. Pure types in lockjaw-types (register offsets, virtqueue layout calculator, feature negotiation model, block request types). Virtqueue runtime in userlib with volatile access and AArch64 memory barriers (dmb ishst/ish/ishld). BlockEngine trait + run_block_server() framework (same pattern as display DDI). Per-device GIC trigger mode (sys_bind_irq flags parameter). Device-manager probe protocol with explicit status codes (PROBE_OK/END/CLAIMED/ERR). Sub-page MMIO offset for virtio-mmio devices (8 per 4K page). Driver selftest reads sector 0 and prints content.

**Phase 15 -- Real Hardware Portability (Raspberry Pi 4B).** Boot path made portable to real AArch64 hardware. Firmware DTB pointer preserved from `x0` at entry. Lightweight FDT platform scanner runs early in boot to discover RAM base/size, UART/GIC/timer MMIO addresses, GIC version (v2 vs v3), and SMP boot method (PSCI vs spin-table) from the device tree. All hardcoded MMIO addresses removed — platform consumers wired to DTB-discovered values. GIC split into v2 and v3 drivers with runtime enum dispatch (Pi 4B uses GICv2; QEMU virt uses GICv3). Position-independent boot with a higher-half pivot — kernel can load at any physical address; `__kernel_start` linker symbol replaces hardcoded `KERNEL_LOAD_ADDR`. DTB-driven SMP boot: PSCI/HVC for QEMU, spin-table (write entry to `cpu-release-addr`, dsb, sev) for Pi 4B. BuddyAllocator capacity bumped from 32 K pages (128 MB) to 262 K pages (1 GB) for real-hardware memory sizes. `core::fmt` replaced with a custom print module — 12% .text savings, removes a class of vtable function pointers in `.rodata` that real-hardware secure boot pipelines would have to allow-list. New `xtask check-vtables` build check scans `.rodata`/`.data` for absolute code pointers and fails the build on unauthorized ones (with an allow-list for legitimate cases like compiler jump tables). FDT parser hardened against real-hardware DTB layouts. `make pi4` produces `kernel8.img` ready to copy to a Pi 4B SD card boot partition.

**Phase 16 -- POSIX Personality (Phases 0-2).** Real musl programs allocate memory and read files end-to-end on Lockjaw. Personality server (`user/posix-server/`) bootstraps with init, parses an embedded ELF, builds the Linux initial stack (argc/argv/auxv with AT_PAGESZ + AT_RANDOM), spawns the child via sys_create_process with a syscall endpoint as handle 0, and dispatches Linux syscalls received over IPC. Three musl patches in `musl-lockjaw/`: `crt_arch.h` (SP adjustment), `syscall_arch.h` (SVC redirect), and `shim.c` (per-syscall dispatch + bootstrap handshake + local brk handling + per-process mmap tracker, with fail-fast `lj_die()` for any transport or bootstrap error). Shared-buffer IPC (one page per client) with the asymmetric Lockjaw reply ABI (messages in x2-x5, reply in x1-x4) used correctly. Real ELF loader handles unaligned LOAD segments. Implemented:

- **Phase 0** (puts via shared buffer): `puts("hello, lockjaw")` from a statically-linked patched-musl binary. write, writev, exit_group, set_tid_address (stub), ioctl (stub), brk (local).
- **Phase 1** (filesystem): `openat / read / close` route through posix-server to a FAT32 filesystem server (`user/fat32-server/`) over a shared-buffer FS-IPC protocol (open/read/close request/reply messages). Per-client OpenTable in fat32-server scoped by caller_token; per-handle DMA buffer PageSet exported to posix-server. fat32-server uses a `BlockEngine`-shaped `BlockClient` to talk to the virtio-blk driver. The FsClient + FdTable infrastructure on the posix-server side mirrors the FdTable shape (caller_token isolation, per-fd resource tracking, deferred-close queue for transport-failure rollback).
- **Phase 2** (mmap + stdio): musl's `malloc` above the brk threshold goes through `mmap(NULL, len, RW, MAP_PRIVATE|MAP_ANONYMOUS)`. Personality server's per-client mmap_table allocates a PageSet, picks a base_va from a bump VA allocator, exports the handle to the client. Shim's failure-ordered handshake (`NR_MMAP` IPC -> `sys_map_pages` -> tracker insert) with explicit `NR_MMAP_ROLLBACK` if any post-export step fails. Variable-size PageSet header (Phase 2.K, see Phase 12) lets one PageSet back up to 64 MiB. Multi-L2 page-table mapping (Phase 2.M) lets a single mapping span multiple L2 regions transactionally (classify, pre-allocate, apply — same shape as consume_pageset_validate/apply). 8 MiB malloc gate verified end-to-end. `fopen + fread + fclose` exercises Phase 1 through musl stdio (which mallocs the FILE struct via mmap), proving Phase 2's mmap-backed malloc supports stdio.
- Phase 3+ (filesystem write, threads via futex, processes via posix_spawn, pipes, signals) still aspirational.

**Phase 17 -- Handle Revocation.** Two-phase consume_pageset (validate + apply) walks every live process's handle table, clears stale exported handles, replaces tombstone-leak pattern. sys_create_process restructured to push every fallible step into the validate phase.

**Phase 18 -- Kernel Objects to KVA.** Every typed kernel object (Endpoint, Notification, Reply, ProcessObject, HandleTable, TCB, per-thread kernel stack) migrated from `page_alloc::alloc_page() + KernelMut::<T>::from_paddr(...)` (the linear higher-half map) to `kvm::alloc_kernel_pages(N) + KernelMut::<T>::from_kva(...)` (a dedicated KVM pool at L0[256]). Each `HandleKind::Foo { paddr }` variant flipped to `HandleKind::Foo { kva }`. Distinct `OwnedKvmRange` / `MappedKvmRange` types make the wrong free path a compile error. Surfaced and fixed a latent POSIX MAP_ANONYMOUS contract bug — `pageset_table::alloc_pages` was returning user-mmap'd frames non-zero, which mallocng's slot-header validation crashed on once the migration shifted which physical frames the buddy hands out at user-mmap time. After the migration, no typed kernel struct is addressed through `+KERNEL_VA_OFFSET` arithmetic; the type system enforces it.

**Phase 19 -- Kernel Image Relink + Pi 4B Bring-Up Validation.** Kernel image relinked at a fixed higher-half VA in a dedicated L0[1] region (`0xFFFF_0080_0000_0000`), decoupled from the physical load address. Boot trampoline discovers actual load PA via PC-relative (`adr _start`) and computes `KERNEL_PHYS_OFFSET = load_PA - LINKER_BASE`. `init_kernel_image_map` walks every kernel image page and writes 4 KB L3 PTEs mapping `load_PA + offset` → `LINKER_BASE + offset` (4 KB granule chosen specifically so any load-PA alignment works, including Pi 4B's `0x80000`). Pivot uses the boot-discovered shift instead of a constant `KERNEL_VA_OFFSET`. New `KernelImageVa` newtype keeps the kernel-image VA regime distinct from the KVM pool's `KernelVa`. New `KernelStackBase` enum (`Image` / `Pool` variants) makes the kernel-stack regime explicit at the type level so `finish_exit`'s free-path choice cannot regress. New `xtask check-linker-symbols` enforces an audit doc listing every linker-symbol-to-integer site with classification. Userspace TTBR0 no longer carries the kernel identity map (L1[1] kernel RAM block + L2[4] device MMIO are gone — userspace device drivers still get MMIO via the normal `sys_map_pages` + `MAP_FLAG_DEVICE` path). Same binary boots end-to-end on QEMU virt and a real Pi 4B (firmware relocates to PA `0x80000`); the only PA-aware input is the DTB.

### Unsafe reduction

The kernel's unsafe usage has been systematically hardened through a multi-round review process with a second AI reviewer (Codex):

- **KernelRef/KernelMut** wrappers concentrate the `paddr + KERNEL_VA_OFFSET` cast in one place. All kernel object field access uses Rust struct syntax, not pointer arithmetic.
- **object_ops facade** provides narrow, operation-level safe methods (send, receive, call, signal, wait) that never expose `&mut T` to callers. Same pattern as CurrentThread.
- **BlockToken** enforces at compile time that no `&mut T` reference to a shared kernel object survives across `block_current()`. The borrow checker prevents moving the token while a scoped reference borrows it.
- **UserAddressSpace** wraps TTBR0 for safe `copy_from_user`. **PageSetRef** wraps validated PageSet IDs. **HandleTableRef** wraps handle table operations.
- Syscall handler has 4 remaining unsafe blocks, all at genuine machine boundaries (page table writes, GIC MMIO, readiness waiter registration, cross-object Reply pointer chasing).

### Testing

Three layers of automated testing run on every build:

| Layer | Count | What it tests |
|-------|-------|---------------|
| Unit tests (host) | 747 | Scheduler model, IPC state machine (exhaustive) + decision functions, process lifecycle + transfer plan + teardown plan, buddy allocator, page tables (PageTableWalk + MapWalk + validate_pte_match + clear_validated_pte + L2RegionIter), ExceptionContext ABI, ESR decode, HandleKind + handle ops + slot_revoke_validate/apply, BackedHeader/BackedHeaderMut wrapper bounds, VirtIO types + layout, block protocol, FDT parser, FAT32 BPB + cluster chains + dirent parser + 8.3 path matching, FS-IPC protocol, POSIX dispatch arms (mmap/munmap/mprotect/madvise + 21 rejection paths) + VA layout + Linux stack writer, device probe protocol, notifications, wait readiness, ticket lock (multi-threaded), feature negotiation, L3 region tracker, ScratchCursor pagination, build_process_page permission policy, KVM allocator + KvmFreeList + KvmMapWalk + KvmFreeWalk, address-regime separation (PhysAddr / KernelVa / KernelImageVa compile_fail doctests) |
| Integration tests (QEMU) | 87 (per GIC variant) | Full boot through 17 phases, scheduler/MMU integration, IPC bootstrap, caller token delivery (positive + negative assertions), thread exit cleanup, thread creation, virtio-blk disk read, FAT32 mount + open + read end-to-end, POSIX Phase 0 puts, Phase 1 file read via fopen, Phase 2.3 malloc(1 MiB) gate, Phase 2.4 malloc(8 MiB) gate, handle revocation diagnostic with multi-process walk assertion |
| Stack analysis | 4 entry points | No recursion, depth within 8KB budget, per-function 1600B cap, all indirect calls annotated, both debug and release profiles |
| Pointer cast lint | 80+ | Every `as *const` / `as *mut` in kernel code has a SAFETY comment |

The IPC state machine test exhaustively explores all reachable system states (endpoint state x per-client reply state x thread states) via BFS with a 3-thread model and verifies: no kernel-caused deadlocks, all 8 invariants hold, all effect orderings correct (BlockCurrent always last, UnblockThread before ClearReply).

### Build tools

**`cargo xtask check-stack`** runs automatically before every `make build` and verifies both debug and release profiles:

- **Four entry points** -- _start, _secondary_start, __vec_sync_lower, __vec_irq (not just the boot path)
- **Combined budget** -- max(normal, secondary) + max(sync exception, IRQ) <= 8192 bytes
- **Per-function cap** -- any single function exceeding 1600 bytes fails immediately
- **No recursion** -- detects cycles in the call graph (DFS on disassembly, allowed_cycles for guarded paths)
- **All indirect calls annotated** -- every `BLR` instruction must be listed in `xtask/stack-annotations.toml` with its known targets, or the build fails
- **Tail call modeling** -- `b <symbol>` parsed as inter-function branches (conservative)
- **No silent gaps** -- missing stack size data is a hard error (assembly functions in `[known_assembly]`, core library with conservative estimates)

**`cargo xtask check-pointers`** runs automatically before every `make build` and verifies:

- **Every pointer cast documented** -- every `as *const` / `as *mut` in `src/` must have a `// SAFETY:` comment explaining why the address is valid (kernel VA, MMIO, linker symbol, etc.)
- Prevents the TTBR0 race class of bugs: user VAs must go through `UserAddressSpace::read`, never raw pointer casts

## Building and running

### Prerequisites

```
rustup target add aarch64-unknown-none
cargo install cargo-binutils rustfilt
rustup component add llvm-tools
brew install qemu  # or apt install qemu-system-aarch64
```

### Build, run, and test

```sh
make build            # Build (runs stack + pointer + vtable checks first)
make run              # Build and run in QEMU
make run-display      # Build and run with ramfb display window
make run-blk          # Build and run with virtio-blk disk (creates test.img)
make pi4              # Build a kernel8.img for Raspberry Pi 4B SD card boot
make test             # Run all tests (unit + integration + stack)
make test-unit        # Host-side unit tests only
make test-qemu        # QEMU integration tests only
make check-stack            # Stack depth and call graph analysis
make check-pointers         # Pointer cast SAFETY annotation check
make check-vtables          # Scan .rodata/.data for unauthorized code pointers
make check-linker-symbols   # Enforce docs/linker-symbol-audit.md allowlist
make objdump                # Disassemble the kernel
```

QEMU is invoked with two UARTs (UART0 for kernel debug, UART1 for userspace driver), GICv3, and a serial mux. Press Ctrl-A then X to exit. `make run-display` adds `-device ramfb -display cocoa` for the framebuffer window. `make run-blk` adds a 1MB virtio-blk device with modern MMIO transport. See `Makefile` for the full command.

## Project structure

```
src/
  main.rs                    # kmain, panic handler, boot banner, init ELF loading
  print.rs                   # kprintln! macro
  crash.rs                   # Crash diagnostics (syscall breadcrumb, thread context)
  percpu.rs                  # Per-CPU data via TPIDR_EL1 (narrow accessors)
  process.rs                 # sys_create_process kernel-side implementation
  elf.rs                     # ELF section lookup for build hash verification
  arch/aarch64/
    boot.rs                  # _start and _secondary_start entry points (EL2→EL1, stack, BSS)
    psci.rs                  # PSCI CPU_ON for secondary core boot (HVC #0)
    uart.rs                  # PL011 UART driver (kernel debug on UART0)
    mmu.rs                   # Boot page tables, MMU enable, higher-half, guard page
    vmem.rs                  # Dynamic per-process page table management
    exceptions.rs            # Exception vectors, crash diagnostics (imports types for ESR decode)
    gic.rs                   # GICv3 interrupt controller
    timer.rs                 # Virtual timer (10ms periodic ticks)
    irq_bind.rs              # IRQ-to-notification binding table
    mod.rs                   # IRQ dispatch (GIC ack, notification signal, timer tick)
  mm/
    addr.rs                  # PhysAddr, PhysPage newtypes, paddr_of_raw
    page_alloc.rs            # Buddy allocator wrapper (single + contiguous allocation)
    kernel_ptr.rs            # KernelRef/KernelMut typed pointer wrappers
    page_table.rs            # PageTableEntry, PageTable types (re-exports from lockjaw-types)
    stack.rs                 # Stack canary init/check
    user_access.rs           # UserAddressSpace, copy_from_user via page table walk (TTBR1)
  cap/
    object.rs                # ObjectType, create-info pattern, query/create
    object_ops.rs            # Safe IPC/notification facade (narrow operation methods)
    handle_table.rs          # HandleTableRef, handle insert/lookup/remove + revoke_validate/apply
    rights.rs                # Rights bitmask
    pageset.rs               # PageSet state machine
    pageset_table.rs         # PageSetRef, PageSet tracking table, contiguous allocation
    revoke.rs                # Cross-process handle revocation (revoke_validate + revoke_apply)
  sched/
    tcb.rs                   # TCB creation (imports Tcb/SavedContext from lockjaw-types)
    context.rs               # context_switch assembly (imports SavedContext from lockjaw-types)
    scheduler.rs             # Per-CPU round-robin scheduler, BlockToken, scoped_mut
    gkl.rs                   # Giant Kernel Lock (wraps TicketLock from lockjaw-types)
    current.rs               # CurrentThread safe facade (narrow per-field accessors)
  ipc/
    endpoint.rs              # Endpoint object, send/receive/call (matches on IPC decisions from types)
    reply.rs                 # Reply object, ipc_reply (matches on ReplyDecision from types)
    notification.rs          # Notification object (timeline semaphore for IRQ delivery)
    ep_queue.rs              # Intrusive FIFO waiter queue on endpoints

lockjaw-types/               # Pure-logic library crate, testable on host
  src/
    addr.rs                  # PhysAddr, PhysPage, PAGE_SIZE
    buddy.rs                 # Buddy allocator (bitmap-per-order, contiguous support)
    page_table.rs            # PageTableEntry, PageTable, PageTableWalk, MapWalk
    rights.rs                # Rights bitmask
    object.rs                # ObjectType, HandleKind enum, HandleEntry, CloseHandleResult, TeardownHandleAction
    handle_ops.rs            # Pure handle-table slot operations (insert/lookup/remove/rights/revoke_validate/revoke_apply)
    virtio.rs                # VirtIO MMIO registers, virtqueue types, block request types, feature negotiation
    block.rs                 # Block device IPC protocol (CMD_GET_INFO/ALLOC_BUFFER/READ/WRITE/FREE_BUFFER)
    ipc_state.rs             # IPC state machine model + kernel-facing decision functions (decide_send/receive/call/reply)
    exception.rs             # ExceptionContext ABI, ESR decode, sync exception classification
    thread.rs                # SavedContext, Tcb, TcbCreateInfo, ThreadBootstrap ABI
    notification_state.rs    # Notification timeline semaphore model
    pageset_table.rs         # PageSet table model + variable-size header (BackedHeader/BackedHeaderMut wrappers), refcount/map_count lifecycle
    fs.rs                    # FS-IPC protocol (open/read/close request/reply messages)
    fat32.rs                 # FAT32 BPB parser, cluster chains, dirent + 8.3 path matching
    posix.rs                 # POSIX dispatch arms (write/openat/read/close/mmap/munmap/mprotect/madvise) + VA layout + Linux initial stack writer
    posix_fd.rs              # Pure FdTable for POSIX fd allocation/lookup/close
    process.rs               # ProcessLifecycle, ProcessTransferPlan, ProcessTeardownPlan
    vmem.rs                  # Page table walk/map validation, index computation
    wait.rs                  # sys_wait_any readiness model, ReadinessWaiter
    fdt.rs                   # Flattened Device Tree parser
    device.rs                # Device types, compatible string hashing, probe/claim protocol constants
    elf.rs                   # ELF64 parser
    syscall.rs               # Syscall numbers (27), SyscallError type, syscall_name()
    constants.rs             # Stack canary, fill pattern, stack base address
    scheduler.rs             # Round-robin scheduling model
    user_pod.rs              # UserPod trait for safe copy_from_user

user/                        # Userspace binaries (separate Cargo projects)
  init/                      # Init process -- spawns and bootstraps all children
  hello/                     # Hello process -- bootstrap protocol test
  uart-driver/               # UART driver -- claims PL011 from device manager
  device-manager/            # Device manager -- DTB parsing, probe/claim-by-addr IPC
  ramfb-driver/              # Display driver -- fw_cfg DMA, contiguous framebuffer
  virtio-blk-driver/         # VirtIO block driver -- MMIO transport, virtqueue, BlockEngine
  display-test/              # Display DDI test client -- queries modes, draws gradient
  fat32-server/              # FAT32 filesystem server -- mounts virtio-blk disk, serves open/read/close over FS-IPC
  fat32-test/                # FAT32 verification client -- end-to-end open + read of /HELLO.TXT
  posix-server/              # POSIX personality server -- ELF loader + Linux syscall dispatch + FsClient + FdTable + mmap_table + per-client VA allocator
  posix-hello/               # POSIX test client -- hello.c (musl-built, fopen + malloc + stdio) + standalone.c (no-libc fallback)
  lockjaw-userlib/           # Shared library (syscalls, display DDI, block DDI, FsClient, virtqueue, PageSetGuard)

musl-lockjaw/                # Patched musl 1.2.5 (downloaded by build.sh)
  build.sh                   # Incremental cross-compile: patches + libc.a + hello.c
  patches/                   # crt_arch.h (SP adjust), syscall_arch.h (SVC redirect)
  src/shim.c                 # lockjaw_syscall: bootstrap handshake + IPC dispatch + local brk

docs/                        # Book of Lockjaw -- design documentation
  book-of-lockjaw/           # Architecture chapters (philosophy + taxonomy)
  patterns/                  # Pattern catalog: 4 shapes for kernel↔types integration
  memory-model.md            # Why the kernel never allocates
  object-model.md            # PageSets, handles, the create-info pattern
  higher-half-kernel.md      # Why the kernel lives in the upper VA half
  kernel-drivers.md          # Why GIC and timer are the only kernel drivers
  threads.md                 # Context switching and preemptive scheduling
  syscalls.md                # Syscall ABI, EL0 drop, yield
  ipc.md                     # IPC design, the two ABIs, message registers
  process-creation.md        # Userspace-driven process creation
  stack-budget.md            # Stack budget analysis and rationale for 8KB
  tech-debt.md               # Known limitations and planned fixes
  extraction-roadmap.md      # Push-shaped code remaining to extract, ranked
  yagni-parking-lot.md       # Removed code tracked for future phases
  development-journal.md     # Journal entries from the AI collaborator (1-10)
  kernel-vmem-roadmap.md     # Kernel VA allocator + relink roadmap
  linker-symbol-audit.md     # Every linker-symbol-to-integer site classified
  relink-notes.md            # Phase 0 diagnostic: what depended on the user-TTBR0 kernel identity
  ben_principles.md          # Personal engineering principles (Tier 1-4)
  handle-revocation-plan.md  # Plan for two-phase consume_pageset + sys_create_process restructure
  posix-musl-plan.md         # Multi-phase plan for the musl personality (Phase 0/1/2 done)
  posix-phase2-mmap-plan.md  # Phase 2 mmap design + sub-phase breakdown

xtask/                       # Build tools
  src/main.rs                # check-stack and check-pointers commands
  stack-annotations.toml     # Indirect call targets for BLR verification

tests/
  qemu_integration.sh        # Boot QEMU, assert expected serial output
```

## Roadmap

| Phase | Status | Description |
|-------|--------|-------------|
| 1. Boot to UART | Done | Bare-metal Rust on QEMU virt, PL011 UART, kprintln!, panic handler |
| 2. Memory Management | Done | Buddy allocator, 4-level page tables, identity + higher-half mapping, guard pages |
| 3. Exceptions and Interrupts | Done | Vector table, GICv3, virtual timer, structured crash diagnostics |
| 4. Kernel Object Model | Done | Vulkan-style create-info, handle tables, rights checking, PageSet donation |
| 5. Threads and Context Switching | Done | TCBs, assembly context_switch, round-robin scheduler, preemptive multithreading |
| 6. Syscall Interface | Done | EL0 userspace, SVC dispatch, typed error returns, PXN/UXN security bits |
| 7. IPC | Done | Synchronous rendezvous, call/reply with per-client Reply objects, non-blocking receive, sys_wait_any |
| 8. Userspace Processes | Done | Per-process TTBR0, ELF loader, init spawns children, bootstrap channel protocol |
| 9. Userspace Drivers | Done | UART driver in userspace, event loop with sys_wait_any, notification-based IRQ delivery |
| 10. Device Manager and Display | Done | DTB parsing, device claim IPC, MMIO PageSets, ramfb driver with DMA framebuffer |
| 11. SMP | Done | Per-CPU stacks, PSCI secondary boot, Giant Kernel Lock, per-CPU scheduler and idle threads |
| 12. PageSet Lifecycle | Done | Mapping tracking, ownership transfer, refcounting, free-on-zero, process exit cleanup |
| 13. Caller Tokens | Done | Kernel-assigned opaque per-endpoint caller tokens for multi-client IPC isolation |
| 14. VirtIO Block Driver | Done | VirtIO MMIO transport, split virtqueue with barriers, block engine + server framework, per-device GIC trigger mode, selftest reads sector 0 |
| 15. Real Hardware Portability | Done (rudimentary Pi 4B) | DTB-driven platform discovery, GICv2/v3 split, PIE boot with higher-half pivot, PSCI + spin-table SMP, custom print (no core::fmt), 1 GB RAM, `make pi4` produces kernel8.img |
| 16. POSIX Phase 0 | Done | Personality server + patched musl + shared-buffer IPC; statically-linked musl `puts("hello, lockjaw")` runs end-to-end |
| 16a. POSIX Phase 1 | Done | FAT32 server (mounts virtio-blk disk), FsClient + FdTable in posix-server, openat/read/close end-to-end; musl direct syscalls read `/HELLO.TXT` |
| 16b. POSIX Phase 2 | Done | Variable-size PageSet header, multi-L2 page-table mapping, posix-server mmap_table + VA allocator, shim mmap/munmap/mprotect/madvise + readv translation; `malloc(8 MiB)` and `fopen + fread + fclose` work end-to-end |
| 17. Handle Revocation | Done | Two-phase consume_pageset (validate + apply) walks every live process's handle table, clears stale exported handles, replaces tombstone-leak pattern. sys_create_process restructured to push every fallible step into the validate phase. |
| 18. Kernel Objects to KVA | Done | Endpoint, Notification, Reply, ProcessObject, HandleTable, TCB, kernel stack all migrated from page_alloc/linear-map to dedicated KVM pool. Distinct OwnedKvmRange / MappedKvmRange types make wrong free path a compile error. Surfaced and fixed latent POSIX MAP_ANONYMOUS zero-init contract bug. |
| 19. Kernel Image Relink + Pi 4B Validation | Done | Linker ORIGIN at L0[1] base (`0xFFFF_0080_0000_0000`), independent of load PA. Boot trampoline maps kernel image PA→VA via 4 KB L3 PTEs (handles any load-PA alignment, including Pi 4B's `0x80000`). User TTBR0 has no kernel entries. Same binary boots on QEMU virt and Pi 4B end-to-end. |
| 20. Architecture Hardening | Ongoing | Extracting pure logic to lockjaw-types (push→pull), making illegal states unrepresentable |
| 21. POSIX Phase 3+ | Planned | Filesystem write, threads (futex), processes (posix_spawn/wait), pipes, signals |

## License

See [LICENSE](LICENSE).
