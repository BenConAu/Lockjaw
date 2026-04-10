# YAGNI Parking Lot

Code that was removed to keep the build warning-free. Each item lists what it was, when it'll be needed, and enough detail to reimplement it quickly.

We follow YAGNI: don't write it until you need it. This doc prevents knowledge loss without the cost of carrying dead code.

---

## VirtAddr newtype (removed from `src/mm/addr.rs`)

**What:** `VirtAddr(u64)` wrapper with `new()`, `as_u64()`, `page_indices()` (extracts L0/L1/L2/L3 indices for 4KB granule 4-level walk), `page_offset()`, `is_page_aligned()`, `Debug`, `LowerHex`.

**When needed:** Phase 6 (userspace) — mapping user pages requires walking the page table by virtual address indices. Also useful for any code that takes a VA and needs to decompose it into table indices.

**Key detail:** `page_indices()` extracts bits [47:39], [38:30], [29:21], [20:12] as L0–L3 indices respectively. `page_offset()` is bits [11:0].

---

## PhysPage::from_number / PhysPage::number (removed from `src/mm/addr.rs`)

**What:** Construct a `PhysPage` from its page number (not address), and retrieve the page number.

**When needed:** If direct page-number manipulation is needed beyond `PhysPage::containing()` and `start_addr()`.

**Key detail:** Page number = `phys_addr >> 12`. `from_number(n)` stores `n` directly; `start_addr()` already exists and does `n << 12`.

---

## PhysAddr::is_page_aligned (removed from `src/mm/addr.rs`)

**What:** `self.0 & (PAGE_SIZE - 1) == 0`

**When needed:** Any phase that adds assertions on page-aligned addresses (mapping, object creation).

---

## AP_RO_EL1, AP_RO_ALL (removed from `src/mm/page_table.rs`)

**What:** Access permission constants for the AP field (bits [7:6]) of page table entries.

- `AP_RO_EL1 = 0b10` — Read-only at EL1, no access at EL0
- `AP_RO_ALL = 0b11` — Read-only at EL1 and EL0

**When needed:** Read-only kernel mappings (`.rodata`, `.text`) need `AP_RO_EL1` for W^X enforcement. Shared read-only user pages need `AP_RO_ALL`.

Note: `AP_RW_ALL` was restored in Phase 6.

---

## SH_OUTER (removed from `src/mm/page_table.rs`)

**What:** `SH_OUTER = 0b10` — Outer Shareable memory attribute.

**When needed:** Multi-core support or specific DMA coherency requirements. Single-core QEMU virt uses Inner Shareable (`SH_INNER`) for normal memory and Non-shareable (`SH_NON`) for device.
