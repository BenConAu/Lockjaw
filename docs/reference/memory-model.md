# Memory Model

Lockjaw's kernel does not dynamically allocate memory. There is no `alloc` crate, no heap, no `malloc`. All kernel data structures are either statically sized (in BSS) or initialized in pages donated by userspace via PageSets.

## Who owns physical memory?

At boot, the kernel knows about all physical RAM (hardcoded for QEMU virt: 128 MB at `0x4000_0000`). Some of it is reserved:

- **Firmware/DTB** — the first 512 KB (`0x4000_0000` to `0x4008_0000`)
- **Kernel image** — `.text`, `.rodata`, `.data`, `.bss` sections
- **Kernel stack** — one 4 KB page, plus a guard page gap
- **Boot page tables** — static arrays in BSS used to set up the MMU

Everything else is **free physical memory** that the kernel will hand to userspace as PageSets.

## The page allocator

The kernel maintains a static bitmap (`[u8; 4096]` in BSS) that tracks which of the 32,768 physical pages (4 KB each) are reserved vs. free. This is **not** a general-purpose allocator — it exists for two purposes:

1. **Boot-time bookkeeping**: track what is reserved so the kernel does not hand out pages that contain its own code or page tables.
2. **PageSet allocation**: when userspace requests physical pages via `sys_alloc_pages`, the kernel allocates from this bitmap and returns a PageSet handle.

Once userspace has PageSets, object creation works like this:

```
Userspace: "I want a new TCB"
         → sys_alloc_pages(query_tcb_size(&info).pages)  // get pages
         → sys_create_tcb(&info, pageset_handle)          // donate pages, create object
         → receives a handle to the new TCB
```

The kernel writes the object into the donated pages. Userspace cannot see the raw memory — it interacts only through handles and syscalls.

## Why not skip the bitmap entirely?

The boot page tables in Milestones 2.4–2.6 are statically allocated (fixed-size arrays in BSS), so they don't need the page allocator. But the page allocator is the backing for all PageSet allocations — it is the mechanism by which userspace obtains physical memory. Building it early also lets us verify that reserved page tracking is correct before the system gets more complex.

## Static kernel memory budget

All kernel-resident memory fits in BSS:

| Item | Size | Notes |
|------|------|-------|
| Page bitmap | 4,096 bytes | 1 bit per 4 KB page, 128 MB RAM |
| Boot page tables | ~24 KB | L0 + L1 + L2 + L3 tables (static arrays) |
| Kernel stack | 4,096 bytes | One page, guard page below |
| BSS zero-init data | varies | Global state, counters, etc. |

No kernel memory grows at runtime. The stack is proven to fit within budget by `cargo xtask check-stack`.
