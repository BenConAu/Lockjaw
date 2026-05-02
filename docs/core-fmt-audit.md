# core::fmt Audit — May 2025

Baseline audit of `core::` library surface in the Lockjaw kernel,
with focus on `core::fmt` as the heaviest dependency.

## core::fmt weight

- **8,972 bytes = 12% of .text** (74,752 bytes total)
- **47 functions** linked into the release binary
- **8 indirect call sites (BLR)** — vtable dispatch inside core::fmt
- Contributes ~400–600 bytes to the 2,624-byte sync exception
  worst-case stack path
- Single vtable in .rodata: `<Uart as core::fmt::Write>` with
  absolute function pointers (broken on non-link-address hardware)

Largest functions by code size:

| Bytes | Function |
|-------|----------|
| 1,804 | `Formatter::pad` |
| 660   | `Formatter::pad_integral` |
| 552   | `PadAdapter::write_str` |
| 496   | `core::fmt::write` |
| 492   | `<&u32 as Debug>::fmt` |
| 412   | `Formatter::debug_struct_field1_finish` |
| 348   | `<i64 as Display>::fmt` |
| 340   | `<u64 as Display>::fmt` |
| 312   | `<HandleKind as Debug>::fmt` |
| 292   | `Formatter::debug_tuple_field1_finish` |

## Format specifiers actually used

The ~80 kprintln! sites use a narrow set:

- `{}` — strings, u8/u32/u64/i64, booleans (Display)
- `{:x}`, `{:#x}`, `{:#018x}`, `{:08x}`, `{:#010x}` — hex with
  padding variants
- `{:02}` — zero-padded decimal (GPR register numbers in exception
  dump)
- `{:?}` — Debug on lockjaw-types enums (8 sites, all diagnostic
  code in kmain)

## The vtable problem

The single `impl core::fmt::Write for Uart` generates a vtable in
.rodata containing absolute function pointers:

```
+24: 0x000000004020c558  write_str
+32: 0x000000004020c5c4  write_char
+40: 0x000000004020c6b8  write_fmt
```

Every path through `core::fmt::write` dispatches via `blr` through
this vtable. On Pi 4B (loaded at 0x80000, linked at 0x40200000),
these pointers are wrong — they point to the link address, not the
physical address. This crashes any formatting that isn't
constant-folded by the compiler.

Working: `kprintln!("decimal: {}", 42u64)` — compiler constant-folds
the format at compile time, emits an inlined puts loop with
PC-relative string reference.

Broken: `kprintln!("hex: {:x}", val)` — goes through
`core::fmt::write` → vtable dispatch → absolute pointer → crash.

## __print fast path vs slow path

The `__print` function has two paths, selected by a tagged pointer
(bit 0 of args):

1. **Bit 0 set** — simple puts loop. Directly writes bytes to UART.
   No vtable, no fmt machinery. Works at any load address.
2. **Bit 0 clear** — calls `core::fmt::write` with the Uart vtable.
   Dispatches through absolute function pointers. Broken on Pi 4B.

The compiler uses the fast path only for constant-folded strings.
Any runtime formatting hits the slow path.

## Other core:: usage (all justified)

| Module | Usage | Assessment |
|--------|-------|------------|
| `core::ptr` | read/write_volatile (MMIO), write/write_bytes (page init), addr_of!/addr_of_mut! (safe statics) | Mandatory for bare-metal |
| `core::cell::UnsafeCell` | All mutable statics (Uart, Scheduler, PageAlloc, BootOnce, PerCpu) | Correct pattern, replaces static mut |
| `core::sync::atomic` | AtomicBool (panic re-entry), AtomicU32 (counters, ticket lock), AtomicU64 (tick counter) | Minimal, all Relaxed except ticket lock |
| `core::mem` | size_of, offset_of, align_of only | No transmute, no zeroed, no MaybeUninit |
| `core::hint` | spin_loop() in UART TX wait, GIC init, ticket lock | Correct for spin-wait |
| `core::arch::asm` | System registers, barriers, WFI | Mandatory |
| `core::slice` | from_raw_parts for DTB/page memory views | Necessary |
| `core::ops::Drop` | 3 RAII guards (HeaderPageGuard, PageGuard, Ttbr0Guard) | Good error-path cleanup |

No alloc:: anywhere. No Box, Vec, String, global allocator.
No transmute, no MaybeUninit, no RefCell in kernel code.

## lockjaw-types fmt surface

- `#[derive(Debug)]` on nearly every public type — pulls in fmt
  code even for types never formatted with `{:?}`
- 4 manual fmt impls: PhysAddr Debug/LowerHex, PhysPage Debug,
  PageTableEntry Debug
- `#[derive(Hash)]` on several types exists only for test-side
  HashSet usage (not needed in kernel)
- The crate has zero dependencies beyond core

## Replacement path

A custom kernel print module covering the actual specifiers used
(decimal, hex with padding, strings, enum variant names) would be:

- ~500–1000 bytes vs 8,972 (>80% reduction)
- Zero indirect calls (all concrete dispatch)
- Zero vtables (no dyn Trait)
- Fully traceable by cargo-call-stack
- Position-independent (no absolute pointers in data)
- Fixes the Pi 4B crash without needing ELF relocation processing

The `core::fmt` machinery (pad, pad_integral, Formatter state,
trait object dispatch) is designed for general-purpose formatting
with arbitrary width/fill/alignment. The kernel needs none of that
generality.
