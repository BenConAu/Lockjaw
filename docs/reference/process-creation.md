# Process Creation

## The problem

To run a new program, someone has to: parse the ELF binary, allocate
physical pages, copy code and data into them, build a page table
mapping those pages at the right virtual addresses, create a thread,
and schedule it. That's a lot of work. The question is: who does it?

## How other systems do it

**Linux:** the kernel does everything. `execve()` reads the ELF
from the filesystem, allocates pages from the kernel's page cache,
builds the user page table, and starts the process. The kernel has
a heap, a VFS, a page cache — it can afford scratch space.

**seL4:** the root task does everything in userspace. The kernel
provides fine-grained syscalls; the root task calls them in a loop,
one page at a time. The kernel never allocates scratch.

**Zircon:** the kernel has a heap and does most of the work,
similar to Linux but with capability checks.

## How Lockjaw does it

Lockjaw follows the seL4 philosophy — the kernel never allocates
memory for its own use — but with a Vulkan-flavored API where the
caller provides all buffers, *plus* a typed plan/apply boundary in
lockjaw-types that the kernel mechanically executes.

### The full flow

The current creator is `user/init/src/main.rs::spawn_elf`. It does
this for each child it spawns:

**Step 1 — parse the ELF (pure, in lockjaw-types).** Init feeds the
child's ELF bytes through `parse_elf` (`lockjaw-types/src/elf.rs:82`),
which returns an `ElfInfo` describing the entry point and PT_LOAD
segments. The parser is shared host-testable code, not a duplicated
implementation in init.

**Step 2 — allocate per-segment pages (userspace -> kernel).** For
each segment, init calls `sys_alloc_pages(count, flags)` (#6). Each
call returns a PageSet handle.

**Step 3 — map and fill (userspace).** Init calls `sys_map_pages`
(#7) to map each PageSet into its own address space at a temporary
VA, then copies the corresponding ELF segment bytes. After this
step, the child's code and data live in init-allocated physical
pages; init's mappings will be unmapped by the kernel during
process creation (the same pages will reappear in the child's
address space at the segment's intended VA).

**Step 4 — build the ProcessMapping list (userspace).** Init writes
a `ProcessMapping` array (`lockjaw-types/src/process.rs:17`) into
scratch — one entry per VA → PageSet mapping the child should have:

```rust
pub struct ProcessMapping {
    pub virt_addr:   u64,
    pub pageset_id:  u64,
    pub page_index:  u64,
    pub flags:       u64,  // bit 0 = PROCESS_MAP_FLAG_EXECUTABLE
}
```

The scratch storage is the same PageSet init will pass to the
kernel as `scratch_pageset_id` in step 5 — userspace owns the
buffer; the kernel only reads from it.

**Step 5 — sys_create_process.** The current signature is 7 args
(`src/syscall/handler.rs:549`):

```rust
sys_create_process(
    mappings_va,             // x0: VA of the ProcessMapping array in init's memory
    mapping_count,           // x1: number of entries
    entry_point,             // x2: VA where the child starts executing
    stack_pageset_id,        // x3: PageSet handle for the child's stack page
    scratch_pageset_id,      // x4: PageSet holding the mapping array itself
    parent_handle_to_copy,   // x5: handle to copy into child slot 0 (u64::MAX = none)
    name,                    // x6: VA of a 16-byte process name string (diagnostic)
)
```

The userlib wrapper at
`user/lockjaw-userlib/src/syscall.rs:116` takes the same arg shape
with typed `PageSetHandle` newtypes.

- **`scratch_pageset_id`** is what makes "kernel never allocates"
  hold for `create_process`'s working-buffer step: it holds the
  per-mapping `Mapping` working buffer that
  `AddressSpaceBuilder::map_batch` consumes in page-sized chunks
  (walked by `ScratchCursor`, see `src/process.rs:404` and the
  flush calls at `:463`/`:500`/`:525`). The dedup'd consumed-headers
  list lives in the child's ProcessObject page
  (`ProcessObject.consumed_headers` at `src/cap/process_obj.rs:44`,
  populated by `process_record_consumed_header` at `:180`) —
  separate storage, in a kernel-allocated KVM-pool page, not
  scratch.
- **`parent_handle_to_copy`** is optional (sentinel `u64::MAX`
  means "no parent copy"). When supplied, the kernel resolves the
  parent's handle in its validate phase and stamps a copy into the
  child's handle table at slot 0 in apply — see
  [`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md)
  for the identity-token rules that govern what kinds of handles
  may be copied.
- **`name`** is a 16-byte string copied into the child's
  ProcessObject for diagnostics; not load-bearing for the runtime.

**Step 6 — kernel runs `create_process`.** The kernel-side
orchestrator (`src/process.rs:152`) follows the plan/apply pattern
from [`../architecture/patterns/plan-apply.md`](../architecture/patterns/plan-apply.md):

1. **Provision.** `provision_resources` (`process.rs:296`) allocates
   the child's ProcessObject, AddressSpaceBuilder, handle table,
   TCB, and TCB stack from KVM/buddy. Records into the plan in the
   actual source order: `plan_builder.record_parent_copy(entry)`
   first (`:356`, if `parent_handle_to_copy != u64::MAX`), then the
   mapping loop calling `record_mapping_into_plan` per entry
   (`:601`, which threads `process_record_consumed_header` for
   dedup), then `record_stack_into_plan` for the stack PageSet
   (`:619`).
2. **Pure structural validate.** `plan_builder.validate(...)`
   consumes the builder, returns a `ValidatedProcessCreationPlan`
   token. Any structural precondition failure (scratch capacity,
   handle table room, scheduler room) aborts here with no apply
   damage.
3. **Kernel-state validate.** Per-header `consume_pageset_validate`
   runs against live revoke/refcount state. The validates are pure
   reads — failures still leave every parent untouched.
4. **Apply.** Per-header `consume_pageset_apply` (the irreversible
   step), then the parent-copy `child_ht.insert(...)`, defuse the
   drop guards, and `scheduler::add_thread(tcb_kva)`. Cannot fail
   under the validate -> apply contract.

The plan-apply doc has the canonical code walk through this flow.

**Step 7 — the child runs.** On the next preemption / scheduler
pick on any CPU, the scheduler swaps to the child's TCB. The
context switch lands in `process_entry` (`src/process.rs:634`),
which releases the GKL and `eret`s to EL0 at the ELF entry point
with the stack set up.

## Teardown

When the last thread of a process exits, the kernel runs the
mirror-image plan/apply teardown:

1. Observe ProcessObject state: `owned_page_count`,
   `has_address_space`, `has_handle_table`, `handle_table_page_count`.
2. Hand those facts to `build_teardown_plan`
   (`lockjaw-types/src/process.rs:436`) which returns a sequence of
   `TeardownStep`s (`:376`):

```rust
pub enum TeardownStep {
    FreeOwnedPages { count: u32 },
    FreeAddressSpace,
    CleanupHandleEntriesPtesGone,
    CleanupHandleEntriesNoAddressSpace,
    FreeHandleTable { page_count: u8 },
    FreeProcessPage,
}
```

3. The kernel iterates `plan.iter()` and matches on each step.
   Sequencing (owned pages before address space, address space
   before handle cleanup, process page last) is encoded in the
   plan's construction order rather than in human memory.

Two distinct cleanup variants for handle entries (`PtesGone` vs
`NoAddressSpace`) instead of a boolean. The construction-safety
property is in the *return type* of each variant's decide function:
`CleanupHandleEntriesNoAddressSpace` routes to
`decide_teardown_handle`, whose return type contains only `DecRef`
and `Skip` — no unmap variant exists in the return type, so the
kernel cannot accidentally attempt an unmap on a kernel process
that has no address space. Source comment at
`lockjaw-types/src/process.rs:386-390` is the authoritative
description.

## Why this design

### The kernel never allocates scratch space

The mapping-list scratch lives in the caller's `scratch_pageset_id`.
The kernel-managed scratch (consumed-headers list, plan-builder
state) lives in the child's ProcessObject page — also allocated as
part of the create flow, not pulled from a kernel heap.

The kernel does allocate KVM-pool pages for the child's
ProcessObject, address space, handle table, and TCB during
`provision_resources`. These are kernel objects in the KVM
allocator's pool ([`memory-model.md`](memory-model.md)), not a
dynamic heap; allocator exhaustion returns an error and the partial
state unwinds via drop guards.

### Each syscall does one bounded thing

- `sys_alloc_pages`: allocate N pages, return a PageSet handle.
- `sys_map_pages`: map one PageSet at one VA.
- `sys_create_process`: build an address space from a mapping list
  + create the first thread + insert into the scheduler. Constant
  stack usage per mapping entry (verified by `cargo xtask check-stack`).

### Userspace does the heavy lifting

Init parses the ELF, decides what pages to allocate, copies the
data, and builds the mapping list. The kernel is a mechanical
executor of the validated plan. Policy in userspace, mechanism in
the kernel.

### Process isolation by construction

The two-phase validate-then-apply structure means the kernel
cannot reach the apply path with an inconsistent plan. Every
fallible step happens in validate; apply is infallible under the
plan-apply contract. The drop-guard scaffolding around the
provisioned resources ensures that any failure between provision
and validate leaves no live kernel-pool pages allocated.

## The Vulkan parallel

The flow mirrors Vulkan GPU resource creation:

| Vulkan | Lockjaw |
|---|---|
| Query buffer size | Query PageSet requirements (per-segment from ELF) |
| Allocate device memory | `sys_alloc_pages` |
| Map memory | `sys_map_pages` |
| Fill buffer with data | Copy ELF segments |
| Create pipeline with buffer references | `sys_create_process` with mapping list + scratch |
| GPU executes | Child process runs |

The caller provides all memory. The kernel never allocates on the
caller's behalf. The same `ProcessMapping` list describes both
"what I want mapped" and "from which PageSets".
