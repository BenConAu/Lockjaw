# Object Model

Lockjaw's object model defines how kernel resources are created, accessed, and secured. It draws inspiration from multiple sources but is its own design.

## The Problem

Every kernel needs objects — threads, IPC endpoints, page tables, memory regions. The question is: who allocates memory for them?

**Traditional kernels (Linux, Windows):** The kernel has an internal heap. When userspace asks for a resource, the kernel mallocs memory from its heap, creates the object, and returns a file descriptor or handle. Userspace has no control over where objects live or how much memory the kernel uses. The kernel can run out of heap memory and fail allocations unpredictably.

**seL4:** Userspace owns *all* physical memory via "Untyped" capabilities. To create a kernel object, userspace "retypes" an untyped region — telling the kernel to initialize an object in memory that userspace already owns. The kernel never allocates. This is powerful and fully deterministic, but complex — userspace must manage memory watermarks and revocation trees.

**Zircon (Fuchsia):** The kernel allocates objects from its own heap, tracks them via reference-counted handles. Simple API, but the kernel can OOM, and userspace has no control over memory placement.

## Lockjaw's Approach

Lockjaw splits the problem into two clean steps, inspired by how Vulkan handles GPU resources:

### Step 1: Allocate physical pages

Userspace asks the kernel for physical pages. The kernel allocates from its page bitmap and returns a **PageSet** — a handle representing 1 to N physical pages.

```
sys_alloc_pages(count: 3) → PageSet handle
```

The kernel tracks which pages are taken. Userspace holds a handle to a set of pages but cannot access them yet.

### Step 2: Choose what to do with the pages

A PageSet has exactly two fates, and they are mutually exclusive:

- **Donate for a kernel object.** The pages become kernel-owned. The kernel initializes an object (handle table, thread, endpoint, etc.) in that memory. Userspace gets a handle to the object but can never see the raw memory again.

- **Map as MappedPages (Phase 6).** The pages are mapped into the process's virtual address space. Userspace gets a pointer and can read/write freely. This is how userspace gets heap memory, buffers, and shared memory regions.

A PageSet cannot be both donated and mapped. This is a security invariant: if userspace could map pages that contain kernel objects, it could overwrite kernel state and escalate privileges.

### One PageSet = One Object

When you donate a PageSet for object creation, the *entire* PageSet is consumed. You cannot donate half a PageSet or create two objects in the same PageSet. This prevents overlap attacks where two objects share memory and one corrupts the other.

A future optimization ("object pools") could pack multiple small objects into a single PageSet, but only if the kernel manages the packing internally — userspace would still see one handle per object.

## The Vulkan Create-Info Pattern

Each object type has its own "create-info" struct that describes the object's configuration. The same struct is used for both querying the required size and creating the object:

```
// Step 1: How much memory?
let info = HandleTableCreateInfo { slot_count: 64 };
let size = query_handle_table_size(&info);    // → 1 page

// Step 2: Allocate pages
let pages = sys_alloc_pages(size.pages);

// Step 3: Donate and create
sys_donate(pages);
sys_create_handle_table(&info, pages) → handle
```

There is no generic `ObjectCreateInfo` — each type has its own struct. This means:
- No ignored fields (unlike a union-style generic struct)
- No runtime dispatch on object type
- Adding a new object type just means adding a new create-info struct and a new pair of query/create functions
- The query and create steps cannot disagree about what is being built, because they use the same struct

## Handles

Once an object is created, userspace interacts with it through an integer **handle**. Handles index into a handle table (itself a kernel object created via PageSet donation). Each handle entry records:

- The physical address of the object
- The object's type
- A rights bitmask (Read, Write, Grant)

Rights are checked on every operation. If you hold a handle with Read but not Grant, you cannot pass that handle to another process.

## Memory Lifecycle

```
Physical pages: free → allocated (PageSet) → donated (kernel object) → destroyed → free
                                            → mapped (MappedPages)   → unmapped  → free
```

Memory always returns to the free pool when the object or mapping is destroyed. The kernel's page bitmap is the single source of truth for physical memory ownership.
