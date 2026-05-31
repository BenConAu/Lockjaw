# Object Model

Lockjaw's object model defines how kernel resources are created,
addressed, and secured. It draws on seL4 (capabilities, no kernel
heap) and Vulkan (typed create-info per object) but is its own design.

## The problem

Every kernel needs objects — threads, IPC endpoints, page tables,
memory regions. The question is: who allocates memory for them?

**Traditional kernels (Linux, Windows):** the kernel has an internal
heap. `malloc` whenever a syscall needs a new object. Userspace has
no control; the kernel can fail allocations unpredictably.

**seL4:** userspace owns *all* physical memory via "Untyped"
capabilities. To create a kernel object, userspace "retypes" an
untyped region — telling the kernel to initialize an object in
memory userspace already owns. The kernel never allocates. Powerful,
fully deterministic; the userspace API is heavy.

**Zircon (Fuchsia):** kernel-allocated, refcounted handles. Simple
API; the kernel can OOM and userspace has no placement control.

## Lockjaw's split

Lockjaw lands in between, with a deliberate split of "where do
pages come from" (userspace allocates) from "where do object bytes
live" (kernel manages):

### Step 1 — userspace allocates a PageSet

```text
sys_alloc_pages(count, flags)            // Buddy-origin PageSet
sys_alloc_dma_pages(count)               // DmaPool-origin PageSet
```

The kernel hands back a handle to a 1..N-page PageSet. Userspace
holds the handle but has not yet decided what the pages are for.

### Step 2 — the PageSet's fate is one of two paths

A PageSet can be **mapped** (becomes user-readable/writable VA
range) or **consumed by object creation** (becomes the storage for
a kernel object). The two paths are mutually exclusive and
construction-enforced: once a PageSet is consumed, its header is
gone and no future `sys_map_pages` can find it; once mapped, the
handle's `mapped_va_page` is non-zero and the consume path rejects
it.

```text
// Fate A: map into the address space
sys_map_pages(pageset_handle, va, flags)

// Fate B: consume into a kernel object — no separate sys_donate
sys_create_endpoint(pageset_handle)      -> EndpointHandle
sys_create_notification(pageset_handle)  -> NotificationHandle
sys_create_reply(pageset_handle)         -> ReplyHandle
```

The earlier two-step `sys_donate` + `sys_create_*(info, pages)` API
described in older versions of this doc is gone; the create syscalls
take the PageSet handle directly.

### One PageSet = one object

When a PageSet is consumed for object creation, the *entire*
PageSet becomes the object's storage. You cannot consume half a
PageSet or create two objects in the same PageSet. This is the
security invariant: two kernel objects can never overlap in memory
(no aliasing == no cross-object corruption).

## The Vulkan create-info pattern (typed configuration)

Where an object has configurable size or per-instance parameters,
its create-info struct lives in `lockjaw-types/src/object.rs` and is
shared between the size-querying path and the creation path. The
canonical example is `HandleTableCreateInfo` (`object.rs:43`):

```rust
pub struct HandleTableCreateInfo {
    pub slot_count: u64,
}
```

The query function answers "how many pages do I need to back a
table of N slots?"; the create function takes the same struct and
initializes the table. Same struct in both, so the query and create
phases cannot disagree about what is being built.

For objects with fixed sizes (Endpoint, Notification, Reply), the
create-info struct is empty (`EndpointCreateInfo` is `;`,
`NotificationCreateInfo` is `;`, etc.) — they exist as type-level
markers for the create-helper signatures, not as runtime
configuration.

## HandleKind: the typed object variants

A `HandleEntry` (`object.rs:150`) contains a `HandleKind` —
each non-empty variant carries the typed address of the underlying
object. The full enum (`object.rs:67`):

```rust
pub enum HandleKind {
    Empty = 0,
    HandleTable        { kva: KernelVa }                                                 = 1,
    ThreadControlBlock { paddr: PhysAddr }                                               = 2,
    Endpoint           { kva: KernelVa, caller_token: Option<NonZeroU64> }               = 3,
    Notification       { kva: KernelVa }                                                 = 4,
    Reply              { kva: KernelVa }                                                 = 5,
    Process            { kva: KernelVa }                                                 = 6,
    PageSet            { kva: KernelVa, mapped_va_page: u32 }                            = 7,
}
```

Two things to notice:

1. **Each variant carries the address regime in its type.** Most
   kernel objects live in the KVM higher-half pool (`KernelVa`); the
   one exception today is `ThreadControlBlock`, which is still a
   buddy-allocated `PhysAddr`. The type system rules out crossing
   the two regimes — a handler that takes `kva: KernelVa` cannot
   accidentally be handed a `PhysAddr`. See
   [`memory-model.md`](memory-model.md) for the KVM allocator;
   [`../tracking/kernel-vmem-roadmap.md`](../tracking/kernel-vmem-roadmap.md)
   tracks the rest-of-objects migration to KVA.
2. **Endpoint carries an identity token.** The `caller_token` field
   distinguishes the master / receive-only handle (`None`) from
   sender handles minted via `sys_export_handle` or by
   `sys_create_process`'s parent-copy path (`Some(t)`). The
   `NonZeroU64` makes a sender-handle-with-token-zero unrepresentable.
   See [`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md)
   for the model.

## Handles, rights, and ownership

Once an object is created, userspace addresses it through a
**handle** — an integer index into the calling thread's handle
table. The HandleTable itself is a kernel object (its
`HandleKind::HandleTable { kva }` variant).

Each `HandleEntry` carries a typed `kind` (above) plus a `Rights`
bitmask (`lockjaw-types/src/rights.rs:4`):

```rust
pub const RIGHT_READ:  u8 = 1 << 0;
pub const RIGHT_WRITE: u8 = 1 << 1;
pub const RIGHT_GRANT: u8 = 1 << 2;
```

Rights are checked on every operation that consumes a handle. A
handle without `RIGHT_GRANT` cannot be passed to another process via
`sys_export_handle`; a handle without `RIGHT_WRITE` cannot be
written through.

Handle table sizing is fixed today: `HANDLE_SLOTS_PER_PAGE = 127`
slots per page (`object.rs:256`), backed by a single page. Multi-page
handle tables are possible per the `HandleTableCreateInfo` shape but
no current syscall path exercises them.

## Revocation

When a PageSet is consumed by another path (or its underlying object
is destroyed), the kernel walks every process's handle table and
clears stale entries. This is `src/cap/revoke.rs`:

```rust
pub fn revoke_validate(header_kva: KernelVa) -> Result<(), RevokeError>;
pub fn revoke_apply(header_kva: KernelVa) -> RevokeStats;
```

The two-phase shape (validate before apply) means a consume that
would leave inconsistent state can be rejected before any change is
made. The full design rationale is in
[`../history/handle-revocation-plan.md`](../history/handle-revocation-plan.md).

## Memory lifecycle

```text
Physical pages: free
            ── sys_alloc_pages / sys_alloc_dma_pages
              -> PageSet (Buddy or DmaPool origin)
                ── sys_create_*(pageset_handle)
                  -> kernel object   ── object destroyed -> free
                ── sys_map_pages
                  -> mapped VA range -> sys_unmap_pages -> free
```

Memory always returns to the originating allocator (buddy or DMA
pool) when the object is destroyed or the mapping unmapped. The
allocators (page bitmap is gone; see
[`memory-model.md`](memory-model.md)) are the single source of
truth for physical memory ownership.
