# Pattern: Pure Data Structures

A type that owns the layout and operations of a data structure, with
no kernel dependencies. The kernel provides the backing memory; the
type provides the methods. Every method is pure: it takes a slice (or
similar borrowed view), reads or mutates it, returns a result.

This is the structural counterpart to [pure decisions](pure-decisions.md):
where pure decisions own *logic*, pure data structures own *layout
and operations*. Reach for it when you have a data structure the
kernel manipulates and the manipulation logic is non-trivial.

## When to reach for it

- The data structure has **layout invariants** worth pinning down
  (which slot is empty, which fields are mutually exclusive,
  alignment requirements).
- The operations on it have **branching logic** the kernel would
  otherwise inline (find empty slot, search by predicate, dedup,
  rights checks).
- The structure lives in **memory the kernel owns** (a donated page,
  an array embedded in a kernel struct) — types can't allocate, but
  it can operate on a `&[T]` or `&mut [T]`.
- You want to **test the operations** without standing up a real
  kernel page.

If the data is just a `repr(C)` blob the kernel reads/writes directly,
you don't need this pattern. If the operations cross structural
boundaries (multiple objects, scheduler state, address space), you
probably want plan/apply or a state machine instead.

## Canonical example: `handle_ops`

The kernel's handle table is an array of `HandleEntry` slots in a
donated page. Lookup, insert, remove, and "find by object" are pure
operations over the slice — no kernel APIs needed once you have the
backing memory.

`handle_ops` (`lockjaw-types/src/handle_ops.rs`) is the shape: a
function library where each function takes a `&[HandleEntry]` or
`&mut [HandleEntry]` and a few pure inputs, returning a `Result`.

**lockjaw-types side** (`lockjaw-types/src/handle_ops.rs:39-94`):

```rust
pub enum HandleError {
    TableFull,
    InvalidHandle,
    InsufficientRights,
}

pub fn find_empty_slot(slots: &[HandleEntry]) -> Option<usize> {
    slots.iter().position(|s| s.object_paddr == 0)
}

pub fn slot_lookup(
    slots: &[HandleEntry],
    index: u32,
    required: Rights,
) -> Result<HandleEntry, HandleError> {
    let slot = slots.get(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    if !slot.rights.contains(required) {
        return Err(HandleError::InsufficientRights);
    }
    Ok(*slot)
}

pub fn slot_insert(
    slots: &mut [HandleEntry],
    object_paddr: u64,
    rights: Rights,
    kind: HandleKind,
) -> Result<u32, HandleError> {
    if matches!(kind, HandleKind::Empty) {
        return Err(HandleError::InvalidHandle);
    }
    let idx = find_empty_slot(slots).ok_or(HandleError::TableFull)?;
    let kind = match kind {
        HandleKind::PageSet { .. } => HandleKind::PageSet { mapped_va_page: 0 },
        other => other,
    };
    slots[idx] = HandleEntry { object_paddr, rights, kind };
    Ok(idx as u32)
}
```

Note the invariant baked into `slot_insert`: when copying a `PageSet`
handle, `mapped_va_page` is forced to 0. Mapping state is per-address-
space, so a copy must not inherit it. Putting that rule in
lockjaw-types means it can't be forgotten by a kernel call site.

**Kernel side** (`src/cap/handle_table.rs:18-66`): a thin wrapper
around a `PhysAddr`, every method delegates to `handle_ops` after
extracting the slice.

```rust
pub struct HandleTableRef(PhysAddr);

impl HandleTableRef {
    pub fn lookup(&self, handle: u32, required_rights: Rights, expected_type: ObjectType)
        -> Result<HandleEntry, SyscallError>
    {
        unsafe {
            let (_header, slots) = table_slots(self.0);
            let entry = handle_ops::slot_lookup(slots, handle, required_rights)
                .map_err(|_| SyscallError::INVALID_HANDLE)?;
            if entry.kind.obj_type() != expected_type {
                return Err(SyscallError::INVALID_PARAMETER);
            }
            Ok(entry)
        }
    }

    pub fn insert(&self, object_paddr: PhysAddr, rights: Rights, kind: HandleKind)
        -> Result<u32, SyscallError>
    {
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_insert(slots, object_paddr.as_u64(), rights, kind)
                .map_err(|e| { /* ... */ SyscallError::HANDLE_TABLE_FULL })
        }
    }
}
```

The kernel's job: turn a `PhysAddr` into a `&mut [HandleEntry]` (the
unsafe `table_slots` helper does this), call the pure function, map
the error vocabulary. Everything that's not memory-safety lives in
lockjaw-types.

The slot operations are exhaustively tested
(`lockjaw-types/src/handle_ops.rs:189-378`): insert into empty tables,
fill sequentially, reuse removed slots, reject empty kinds, rights
subset checks, mapped-va round trips. The tests build a
`Vec<HandleEntry>` on the host — no kernel page needed.

## Variants

### Methods on a layout type: `PageTableEntry`

`PageTableEntry` (`lockjaw-types/src/page_table.rs:57-149`) is the
finer-grained version of this pattern: a single `repr(transparent)
u64` with const-fn accessors for every bit field
(`is_valid`, `is_table`, `is_block`, `attr_index`, `ap`, `sh`, `af`,
`output_addr`, `is_pxn`, `is_uxn`). Constructors (`new_page`,
`new_block`, `new_table`) and decorators (`with_pxn`, `with_uxn`)
return new entries. The kernel never bit-twiddles a u64 directly.

Same shape as `handle_ops` — the type owns layout, the kernel calls
methods. Smaller scale: one entry instead of an array.

### Pure query over a closure-fed view: `query_mapping_run`

`query_mapping_run` (`lockjaw-types/src/page_table.rs:395-484`) is a
hybrid. The "data structure" is a multi-level page table tree the
kernel can't directly slice. Instead, the kernel passes a
`read_pte: Fn(u64) -> u64` closure, and the function walks the tree
through that capability. All PTE interpretation lives in lockjaw-types;
the kernel only provides the bytes.

Same pattern as a state machine, but flatter: one function call
instead of a step loop. Use this when the data structure is too
distributed to expose as a `&[T]` but the operations are still pure.

### Plan-builder methods: `ProcessTransferPlan`

`ProcessTransferPlan` (`lockjaw-types/src/process.rs:130-207`) is a
data structure with operations (`add_header`, `record_unmap`,
`validate`, `headers`). The kernel calls each method; types
maintains the invariants (dedup, capacity, validation).

This overlaps with [plan/apply](plan-apply.md) — a plan *is* a pure
data structure. The shape distinction: plan/apply wraps a build →
validate → apply protocol; pure-data-structure exposes a flat method
surface without that lifecycle. Same techniques apply.

## Common pitfalls

- **Forgetting that the kernel owns memory.** lockjaw-types doesn't
  allocate. If your operation conceptually wants to "grow the table,"
  the kernel needs to allocate the backing pages and pass in a fresh
  larger slice. Methods take what they need; they can't make more.

- **Per-context invariants on shared types.** `slot_insert` zeroes
  `mapped_va_page` on PageSet copies because mapping state is
  per-address-space. The bug class this prevents is "I exported a
  handle and now the recipient sees the sender's mapped VA." Surface
  invariants like this *in the operation*, not in a comment at the
  call site.

- **Layout drift between types and kernel.** The `repr(C)` layout in
  lockjaw-types is the source of truth. If the kernel does manual
  offset arithmetic to read a field, that's a duplicate definition
  waiting to drift. Always go through accessor methods. The
  `HandleTableHeader % 8 == 0` static assertion in
  `src/cap/handle_table.rs:161` is a guard against alignment drift —
  good pattern to copy.

- **Returning references that outlive the slice.** Pure functions over
  slices can return `Result<&T, _>` — but the borrow has the slice's
  lifetime. The kernel's `unsafe` block can synthesize a `'static`
  lifetime if needed (see `table_slots` returning
  `&'static mut [HandleEntry]`), but only because the underlying page
  is owned for the lifetime of the table. Be deliberate about this:
  the type doesn't know how long its slice lives, so the borrow is
  the kernel's contract to honor.

- **Kernel error vocabulary leaking in.** `handle_ops::HandleError` is
  a small lockjaw-types-side enum (3 variants). The kernel maps it to
  `SyscallError::INVALID_HANDLE` etc. at the boundary. If the type
  starts returning `SyscallError`, lockjaw-types becomes a kernel
  dependency. Keep the error type narrow and pure.

## Recognizing push-shaped code that wants this pattern

You're looking at an opportunity when:

- A data structure is defined as a `repr(C)` blob, but the operations
  on it are open-coded inline in kernel functions.
- Multiple kernel call sites duplicate the same scan/lookup/match
  logic over the same array.
- A bug in the data structure's operations would be hard to test
  because the structure normally lives in a kernel-owned page.
- The data structure's invariants are documented in comments rather
  than enforced by methods.

The refactor: define the data structure (or import its `repr(C)` form)
in lockjaw-types. Move every operation that doesn't need a real
kernel page into pure functions taking `&[T]` or `&mut [T]`. Keep
unsafe in the kernel — types should never need it. Replace the kernel
call sites with thin wrappers that obtain the slice and delegate.
