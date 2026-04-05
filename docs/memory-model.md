# Memory Model

Lockjaw follows the seL4 memory model: **the kernel does not dynamically allocate memory**. There is no `alloc` crate, no heap, no `malloc`. All kernel data structures are either statically sized (in BSS) or carved from user-provided memory via the capability system.

## Who owns physical memory?

At boot, the kernel knows about all physical RAM (hardcoded for QEMU virt: 128 MB at `0x4000_0000`). Some of it is reserved:

- **Firmware/DTB** — the first 512 KB (`0x4000_0000` to `0x4008_0000`)
- **Kernel image** — `.text`, `.rodata`, `.data`, `.bss` sections
- **Kernel stack** — one 4 KB page, plus a guard page gap
- **Boot page tables** — static arrays in BSS used to set up the MMU

Everything else is **free physical memory** that the kernel will hand to userspace.

## The bitmap frame allocator

The kernel maintains a static bitmap (`[u8; 4096]` in BSS) that tracks which of the 32,768 physical frames (4 KB each) are reserved vs. free. This is **not** a general-purpose allocator — it exists for two narrow purposes:

1. **Boot-time bookkeeping**: track what's reserved so the kernel doesn't hand out frames that contain its own code or page tables.
2. **Untyped capability creation (Phase 4)**: when the init process starts, all remaining free frames are wrapped into Untyped capabilities and given to it. After that, the kernel's frame allocator has done its job.

Once userspace has all the Untypeds, memory allocation works like this:

```
Userspace: "I want a new TCB"
         → sys_retype(untyped_cap, ObjectType::TCB, size)

Kernel:   Carves bytes from the Untyped's region (watermark allocator)
         → Returns a new TCB capability to the caller
```

The kernel never calls `alloc_frame()` after boot. Userspace drives all memory allocation through the Retype syscall, and the kernel just does the bookkeeping.

## Why not skip the bitmap entirely?

The boot page tables in Milestones 2.4–2.6 are statically allocated (fixed-size arrays in BSS), so they don't need the frame allocator. But Phase 4 needs the bitmap to know which frames are free when constructing the initial Untyped capabilities for the init process. Building it early also lets us verify that reserved frame tracking is correct before the system gets more complex.

## Static kernel memory budget

All kernel-resident memory fits in BSS:

| Item | Size | Notes |
|------|------|-------|
| Frame bitmap | 4,096 bytes | 1 bit per 4 KB frame, 128 MB RAM |
| Boot page tables | ~24 KB | L0 + L1 + L2 + L3 tables (static arrays) |
| Kernel stack | 4,096 bytes | One page, guard page below |
| BSS zero-init data | varies | Global state, counters, etc. |

No kernel memory grows at runtime. The stack is proven to fit within budget by `cargo xtask check-stack`.
