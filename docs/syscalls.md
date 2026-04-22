# Syscall Interface

## Why EL0 matters

AArch64 has four Exception Levels (EL0-EL3). Lockjaw runs the kernel at EL1 and userspace at EL0. The privilege boundary between them is enforced by the CPU hardware:

- **EL0 code cannot access kernel memory.** Page table entries have access permission bits (AP field) that distinguish kernel-only (AP=0b00) from user-accessible (AP=0b01). An EL0 access to a kernel-only page triggers a data abort.
- **EL0 code cannot execute privileged instructions.** MSR/MRS to system registers, ERET, TTBR writes — all trap if attempted from EL0.
- **The only way for EL0 to talk to the kernel is SVC.** The `SVC #0` instruction generates a synchronous exception, which traps to the kernel's exception vector table. The kernel handles the request and returns to EL0 via `ERET`.

Without this separation, a buggy or malicious userspace program could overwrite kernel data structures, disable interrupts, or take over the machine.

## The syscall convention

Lockjaw uses a simple register-based calling convention for syscalls:

```
x8     = syscall number
x0-x5  = arguments (up to 6)
SVC #0 = trap instruction

After return:
x0     = return value (0 = success, u64::MAX = error)
```

This is similar to Linux's AArch64 syscall convention (which also uses x8 for the number and x0-x5 for arguments). The choice of x8 keeps x0 free for both the first argument and the return value.

## How the trap works

When userspace executes `SVC #0`, the CPU:

1. Saves the current PC to `ELR_EL1` (Exception Link Register)
2. Saves the current PSTATE to `SPSR_EL1` (Saved Program Status Register)
3. Switches to EL1 and jumps to the exception vector at `VBAR_EL1 + 0x400` (Lower EL, AArch64, Synchronous)

The vector stub saves all 31 general-purpose registers plus ELR/SPSR/ESR onto the kernel stack (272 bytes), then calls the Rust handler with a pointer to this saved context.

The handler reads x8 from the saved context, dispatches to the right syscall function, writes the return value into the saved x0, and returns. The assembly stub restores all registers from the stack and executes `ERET`, which returns to EL0 at the instruction after the SVC.

## Separate exception vectors for userspace

The exception vector table has separate groups for "Current EL" (kernel faults) and "Lower EL" (userspace traps). Before Phase 6, all groups pointed to the same handlers. Now:

- **Offset 0x200 (Current EL, Synchronous):** Kernel fault handler — prints details and halts. This fires if the kernel itself hits a data abort, undefined instruction, etc.
- **Offset 0x400 (Lower EL, Synchronous):** Syscall dispatcher. Checks the Exception Class (EC field of ESR_EL1):
  - EC 0x15 = SVC from AArch64 → dispatch syscall
  - Anything else = userspace fault → print and halt the thread

The IRQ vector (offset 0x480) is shared — timer interrupts work the same regardless of whether they preempt kernel or userspace code.

## The EL1 to EL0 drop

To start running user code, the kernel:

1. Sets up a user page table in TTBR0 with user-accessible pages
2. Writes the user entry point to `ELR_EL1`
3. Writes `SPSR_EL1 = 0` (EL0t mode, interrupts enabled)
4. Writes the user stack pointer to `SP_EL0`
5. Executes `ERET` to drop to EL0

Both TTBR0 (user pages) and TTBR1 (kernel pages) are active simultaneously. User code uses TTBR0 addresses. When an exception occurs, the CPU switches to EL1 and the kernel accesses its code and data via TTBR1 (higher-half addresses) — or via the identity map that is temporarily included in TTBR0 (see the identity map workaround below).

## The identity map workaround

The kernel binary is linked at physical addresses (0x40080000) by the linker script. VBAR_EL1 and all function addresses point to these physical addresses. When TTBR0 is replaced with user page tables, the kernel's physical addresses must still be reachable for exception handling to work.

The current workaround: include the kernel's identity map (RAM and device MMIO) as kernel-only entries (AP_RW_EL1) in the user TTBR0. Userspace cannot access these ranges — they fault with a permission error. This is safe but not pure: ideally the kernel would be linked at higher-half VAs and TTBR0 would contain only user mappings.

## Why yield exists

`sys_yield` (syscall 1) is a permanent part of the syscall API. It voluntarily reschedules the calling thread — the thread goes to the back of the run queue and the next ready thread runs.

Every microkernel has this:
- seL4: `seL4_Yield`
- Zircon: `zx_thread_yield`
- Linux: `sched_yield`

Without yield, a thread waiting for something would spin-wait (burning CPU) until the 10ms timer fires and preempts it. With yield, the thread says "I have nothing useful to do right now" and the scheduler immediately picks someone else. This matters for:

- **Server processes** that handle requests and want to return their time slice after replying
- **Polling patterns** where a thread checks a condition and yields between attempts
- **Fairness** between threads with different workloads

## Current syscalls (22)

| # | Name | Arguments | Returns | Description |
|---|------|-----------|---------|-------------|
| 0 | debug_putc | x0=char | — | Print one character via UART (debug only) |
| 1 | yield | — | — | Voluntary reschedule |
| 2 | send | x0=ep handle, x1-x4=msg | — | Send message on endpoint (non-blocking) |
| 3 | receive | x0=ep handle | x1-x4=msg | Receive message (blocks if queue empty) |
| 4 | call | x0=ep handle, x1=reply handle, x2-x5=msg | x1-x4=reply | Send + block for reply (call/reply IPC) |
| 5 | reply | x0-x3=msg | — | Reply to current bound caller |
| 6 | alloc_pages | x0=count, x1=flags | x1=PageSet ID | Allocate physical pages (flag 1 = contiguous) |
| 7 | map_pages | x0=PageSet ID, x1=VA, x2=flags | — | Map pages into caller's address space |
| 8 | create_process | x0=mappings ptr, x1=count, x2=entry, x3=stack ps, x4=scratch ps, x5=handle, x6=name ptr | — | Create new process from mapping list |
| 9 | create_notification | x0=PageSet ID | x1=handle | Create notification (timeline semaphore) |
| 10 | signal_notification | x0=handle, x1=value | — | Signal notification (must be monotonic) |
| 11 | wait_notification | x0=handle, x1=threshold | x1=value | Wait until counter >= threshold |
| 12 | bind_irq | x0=INTID, x1=notif handle | — | Bind hardware IRQ to notification |
| 13 | create_endpoint | x0=PageSet ID | x1=handle | Create IPC endpoint |
| 14 | recv_nb | x0=ep handle | x1-x4=msg | Non-blocking receive (WOULD_BLOCK if empty) |
| 15 | wait_any | x0=entries ptr, x1=count | x1=ready bitmask | Wait on multiple endpoints/notifications |
| 16 | export_handle | x0=handle | x1=new index | Duplicate handle into bound caller's table |
| 17 | get_boot_info | — | x1=DTB PageSet ID | Get boot information (DTB location) |
| 18 | register_device_page | x0=phys addr | x1=PageSet ID | Register MMIO page as tracked PageSet |
| 19 | query_pageset_phys | x0=PageSet ID, x1=page index | x1=phys addr | Query physical address of a page |
| 20 | create_reply | x0=PageSet ID | x1=handle | Create Reply object for call/reply IPC |
| 21 | exit | — | (never returns) | Exit current thread, free resources |
| 22 | create_thread | x0=entry, x1=stack_top, x2=stack_base, x3=arg | — | Create thread in calling process (shares address space). VA range validated; mapping not checked (faults at EL0 if unmapped). stack_top must be 16-byte aligned. |
