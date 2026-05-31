# Process Creation

## The problem

To run a new program, someone has to: parse the ELF binary, allocate physical pages, copy code and data into them, build a page table mapping those pages at the right virtual addresses, create a thread, and schedule it. That's a lot of work. The question is: who does it?

## How other systems do it

**Linux:** The kernel does everything. `execve()` reads the ELF from the filesystem, allocates pages from the kernel's page cache, builds the user page table, and starts the process. The kernel has a heap, a VFS layer, and a page cache — it can afford to allocate scratch space internally.

**seL4:** The root task (init) does everything in userspace. The kernel provides fine-grained syscalls: create a page table object, map one page, create a thread. The root task calls these in a loop, one page at a time. The kernel never allocates scratch space because each syscall does one small thing with constant stack usage.

**Zircon:** The kernel has a heap and does most of the work, similar to Linux but with capability checks.

## How Lockjaw does it

Lockjaw follows the seL4 philosophy — the kernel never allocates memory for its own use — but with a Vulkan-flavored API where the caller provides all buffers.

### The full flow

Init wants to spawn a child process from an embedded ELF binary. Here is exactly what happens:

**Step 1: Init parses the ELF (userspace)**

Init has the child's ELF binary embedded via `include_bytes!`. It parses the ELF64 header and program headers in its own code, extracting: entry point VA, and for each PT_LOAD segment: virtual address, file offset, file size, memory size, and flags.

The ELF parser is ~80 lines of byte-level parsing. No shared library with the kernel — init has its own parser.

**Step 2: Init allocates pages for each segment (userspace → kernel)**

For each page needed by each segment, init calls:
```
sys_alloc_pages(1) → PageSet ID
```

The kernel allocates one physical page from its bitmap and returns an ID. Init calls this in a loop — one page per call, constant kernel stack usage.

**Step 3: Init maps pages and copies data (userspace)**

For each allocated page, init calls:
```
sys_map_pages(pageset_id, temp_va) → success
```

This maps the physical page into init's own address space at a temporary VA. Init can now write to it. Init zeroes the page, then copies the relevant ELF segment data from the embedded blob.

After this step, the child's code and data are in physical pages that init allocated. The pages are mapped in init's address space (for writing) but will also be mapped in the child's address space (for execution).

**Step 4: Init builds the mapping list (userspace)**

Init allocates another page for a `ProcessMapping` array — a list of (virtual address, PageSet ID, page index, flags) entries. One entry per page. This array lives in init's own mapped memory.

**Step 5: Init calls sys_create_process (userspace → kernel)**

```
sys_create_process(
    mappings_ptr,       // pointer to the mapping array in init's memory
    mapping_count,      // number of entries
    entry_point,        // VA where the child starts executing
    stack_pageset_id,   // PageSet ID for the child's stack page
)
```

**Step 6: The kernel creates the child (kernel)**

The kernel reads the mapping array from init's memory one entry at a time (init's TTBR0 is still active, so user pointers work). For each entry:
- Resolves the PageSet ID to a physical address via the PageSet tracking table
- Adds a page table entry in the child's new L0→L1→L2→L3 hierarchy

The kernel also:
- Allocates page table pages (L0, L1, L2, L3) from the page bitmap — these are kernel objects, like HandleTables and TCBs
- Creates a handle table for the child
- Creates a TCB with the child's TTBR0, entry point, and stack pointer
- Adds the child to the scheduler's run queue

**Step 7: The child runs**

On the next timer tick (or when init yields), the scheduler picks the child thread. It swaps TTBR0 to the child's page table and context-switches. The child's `process_entry` function drops to EL0 at the ELF entry point. The child starts executing its own code at VA 0x400000 — the same virtual address as init, but backed by different physical pages.

## Why this design

### The kernel never allocates scratch space

The previous attempt at process creation had the kernel allocating a scratch page for the mapping array. This violated the "kernel never allocates" principle. The current design puts the mapping array in init's memory — init allocates the page, init writes the entries, the kernel just reads them.

The kernel still allocates page table pages (L0/L1/L2/L3) during `create_address_space`, but these are kernel objects — the same category as HandleTables and TCBs. They're allocated from the page bitmap, not from a kernel heap. The bitmap is a fixed-size data structure in BSS, not dynamic allocation.

### Each syscall does one small thing

- `sys_alloc_pages`: allocate N pages, return an ID
- `sys_map_pages`: map one PageSet at one VA
- `sys_create_process`: build an address space from a mapping list

No single syscall needs variable-sized scratch space. The mapping list in `sys_create_process` is read from userspace memory — the kernel processes it one entry at a time with constant stack usage (verified by `cargo xtask check-stack`).

### Userspace does the heavy lifting

Init parses the ELF, decides what pages to allocate, copies the data, and builds the mapping list. The kernel is a dumb executor: "map this physical page at this VA, create a thread at this entry point." This is the microkernel philosophy — policy in userspace, mechanism in the kernel.

### Process isolation is proven by construction

Both init and hello are linked at VA 0x400000. They run at the same virtual address. If they shared an address space, the child would execute init's code. The fact that the child prints "Hello from child process!" (not "Hello from userspace init!") proves they have separate address spaces with separate page tables mapping the same VA to different physical pages.

## The Vulkan parallel

This whole flow mirrors how Vulkan handles GPU resource creation:

| Vulkan | Lockjaw |
|--------|---------|
| Query buffer size | Query PageSet requirements |
| Allocate device memory | sys_alloc_pages |
| Map memory | sys_map_pages |
| Fill buffer with data | Copy ELF segments |
| Create pipeline with buffer references | sys_create_process with mapping list |
| GPU executes | Child process runs |

The caller provides all memory. The driver/kernel never allocates on the caller's behalf. The same create-info struct (or mapping list) describes both "what I want" and "where I put the resources."
