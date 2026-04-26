# Stack Budget

Each CPU gets an 8 KB kernel stack with a 4 KB guard page (unmapped).
The stack checker (`cargo xtask check-stack`) verifies both debug and
release builds against this budget.

## Layout (per CPU)

```
+------------------+
| Guard page (4K)  |  Unmapped — fault on overflow
+------------------+
| Kernel stack     |
|     (8 KB)       |  SP starts at top, grows down
+------------------+
```

4 CPUs x 12 KB = 48 KB total (2 MB-aligned for guard page mapping).

## Budget breakdown

The checker computes worst-case stack depth for four paths:

| Path | Entry point | What it covers |
|------|-------------|----------------|
| Normal | `_start` | Primary boot: kmain init, scheduler setup |
| Secondary | `_secondary_start` | AP bring-up: per-CPU init, scheduler entry |
| Sync exception | `__vec_sync_lower` | Userspace syscalls and faults |
| IRQ exception | `__vec_irq` | Timer ticks, device interrupts |

**Combined constraint**: `max(normal, secondary) + max(sync, irq) <= 8192`

This is conservative — sync exceptions from userspace start on a clean
kernel stack (they don't nest on top of normal), and IRQs are masked
during both boot and syscall handling (GKL held). The real worst case
is narrower than what the checker enforces.

## Per-function cap: 1536 bytes

Any single function with a frame exceeding 1536 bytes fails the check.
This catches large stack-allocated arrays before they interact with
call depth (the bug class that motivated this checker).

## Why 8 KB

4 KB was tight. The debug-build boot path (`_start` -> `kmain`) alone
uses ~2700 bytes because kmain has many locals that can't be static:

- `dtb_pages: [PhysAddr; 16]` (128B) — DTB page addresses computed
  from the DTB header at runtime
- `pages: [Option<Page>; 10]` (~160B) — boot-time page allocator
  verification
- `guard_pages: [PhysAddr; 4]` (32B) — linker symbol addresses
- Linker symbol pointers, handle table setup, PTE verification locals
- Debug build overhead: no stack slot reuse, alignment padding,
  compiler-inserted assertion temporaries

These all exist only during boot (IRQs masked). After boot, kmain
enters the scheduler and the frame is gone. But the checker
conservatively assumes exceptions can nest at the deepest point.

With 8 KB the combined budget has comfortable headroom even in debug
builds, while the guard page still catches real overflows.

## Annotations

The checker uses `xtask/stack-annotations.toml` for:

- **`[indirect_calls]`** — maps BLR (indirect call) sites to their
  known targets. Every BLR must be listed or the check fails.
- **`[known_assembly]`** — manually measured frame sizes for
  assembly-only functions and core library functions that lack
  `emit-stack-sizes` data (precompiled sysroot).
- **`[allowed_cycles]`** — functions with runtime re-entry guards
  where the compiler sees a call-graph cycle but recursion can't
  actually happen (e.g. panic handler formatting).
