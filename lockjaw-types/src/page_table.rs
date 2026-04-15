use crate::addr::PhysAddr;
use core::fmt;

// ---------------------------------------------------------------------------
// Memory attribute constants
// ---------------------------------------------------------------------------

/// MAIR_EL1 attribute index 0: Device-nGnRnE (strongly ordered device memory).
pub const MAIR_DEVICE: u8 = 0;

/// MAIR_EL1 attribute index 1: Normal, Inner/Outer Write-Back Read/Write-Allocate.
pub const MAIR_NORMAL: u8 = 1;

/// MAIR_EL1 register value.
///   Attr[0] (bits  7:0) = 0x00: Device-nGnRnE
///   Attr[1] (bits 15:8) = 0xFF: Normal WB RA WA (inner + outer)
pub const MAIR_EL1_VALUE: u64 = (0x00 << 0) | (0xFF << 8);

// ---------------------------------------------------------------------------
// Access permission constants (AP field, bits [7:6])
// ---------------------------------------------------------------------------

/// Read-write at EL1, no access at EL0.
pub const AP_RW_EL1: u8 = 0b00;

/// Read-write at EL1 and EL0.
pub const AP_RW_ALL: u8 = 0b01;

// ---------------------------------------------------------------------------
// Shareability constants (SH field, bits [9:8])
// ---------------------------------------------------------------------------

pub const SH_NON: u8 = 0b00;
pub const SH_INNER: u8 = 0b11;

// ---------------------------------------------------------------------------
// Page table entry bit positions
// ---------------------------------------------------------------------------

const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1;
const PTE_AF: u64 = 1 << 10;
const PTE_PXN: u64 = 1 << 53;
const PTE_UXN: u64 = 1 << 54;

const PTE_ATTR_SHIFT: u64 = 2;
const PTE_AP_SHIFT: u64 = 6;
const PTE_SH_SHIFT: u64 = 8;

/// Mask for the output address field (bits [47:12]).
const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// ---------------------------------------------------------------------------
// PageTableEntry
// ---------------------------------------------------------------------------

/// A single 64-bit AArch64 page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn is_valid(self) -> bool {
        self.0 & PTE_VALID != 0
    }

    pub const fn is_table(self) -> bool {
        self.0 & (PTE_VALID | PTE_TABLE) == (PTE_VALID | PTE_TABLE)
    }

    pub const fn is_block(self) -> bool {
        self.0 & (PTE_VALID | PTE_TABLE) == PTE_VALID
    }

    pub const fn attr_index(self) -> u8 {
        ((self.0 >> PTE_ATTR_SHIFT) & 0x7) as u8
    }

    pub const fn ap(self) -> u8 {
        ((self.0 >> PTE_AP_SHIFT) & 0x3) as u8
    }

    pub const fn sh(self) -> u8 {
        ((self.0 >> PTE_SH_SHIFT) & 0x3) as u8
    }

    pub const fn af(self) -> bool {
        self.0 & PTE_AF != 0
    }

    pub const fn output_addr(self) -> PhysAddr {
        PhysAddr::new(self.0 & PTE_ADDR_MASK)
    }

    pub const fn new_table(next_table_phys: PhysAddr) -> Self {
        Self(PTE_VALID | PTE_TABLE | (next_table_phys.as_u64() & PTE_ADDR_MASK))
    }

    pub const fn new_block(phys: PhysAddr, attr: u8, ap: u8, sh: u8) -> Self {
        Self(
            PTE_VALID
                | ((attr as u64 & 0x7) << PTE_ATTR_SHIFT)
                | ((ap as u64 & 0x3) << PTE_AP_SHIFT)
                | ((sh as u64 & 0x3) << PTE_SH_SHIFT)
                | PTE_AF
                | (phys.as_u64() & PTE_ADDR_MASK),
        )
    }

    pub const fn new_page(phys: PhysAddr, attr: u8, ap: u8, sh: u8) -> Self {
        Self(
            PTE_VALID
                | PTE_TABLE
                | ((attr as u64 & 0x7) << PTE_ATTR_SHIFT)
                | ((ap as u64 & 0x3) << PTE_AP_SHIFT)
                | ((sh as u64 & 0x3) << PTE_SH_SHIFT)
                | PTE_AF
                | (phys.as_u64() & PTE_ADDR_MASK),
        )
    }

    pub const fn with_pxn(self) -> Self {
        Self(self.0 | PTE_PXN)
    }

    pub const fn with_uxn(self) -> Self {
        Self(self.0 | PTE_UXN)
    }
}

impl fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.is_valid() {
            write!(f, "PTE(invalid)")
        } else {
            write!(
                f,
                "PTE(addr={:#x} attr={} ap={:#04b} sh={:#04b} af={})",
                self.output_addr().as_u64(),
                self.attr_index(),
                self.ap(),
                self.sh(),
                self.af(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// PageTable
// ---------------------------------------------------------------------------

/// A 4 KB-aligned page table containing 512 entries.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); 512],
        }
    }
}

// ---------------------------------------------------------------------------
// Page table walk state machine
// ---------------------------------------------------------------------------

/// Result of each step in a page table walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkResult {
    /// Read the u64 at this physical address and pass it to step().
    Continue(u64),
    /// Walk complete: the VA maps to this physical address.
    Done(u64),
    /// Walk faulted: the VA is unmapped at some level.
    Fault,
}

/// Pure page table walk state machine. The kernel feeds raw PTE values
/// read from memory; this module handles all PTE interpretation.
///
/// Usage:
/// ```ignore
/// let (mut walk, mut result) = PageTableWalk::start(ttbr0_paddr, user_va);
/// loop {
///     match result {
///         WalkResult::Continue(pte_paddr) => {
///             let pte_raw = read_phys(pte_paddr); // kernel reads via TTBR1
///             result = walk.step(pte_raw);
///         }
///         WalkResult::Done(phys_addr) => return Some(phys_addr),
///         WalkResult::Fault => return None,
///     }
/// }
/// ```
pub struct PageTableWalk {
    level: u8,
    va: u64,
    indices: [usize; 4],
}

impl PageTableWalk {
    /// Begin a walk. Returns the walker and the first physical address to read.
    pub fn start(ttbr0_paddr: u64, va: u64) -> (Self, WalkResult) {
        let (l0, l1, l2, l3) = crate::vmem::page_table_indices(va);
        let walk = Self {
            level: 0,
            va,
            indices: [l0, l1, l2, l3],
        };
        let pte_paddr = ttbr0_paddr + (l0 as u64) * 8;
        (walk, WalkResult::Continue(pte_paddr))
    }

    /// Feed a raw PTE value read from the address returned by the previous step.
    pub fn step(&mut self, pte_raw: u64) -> WalkResult {
        let pte = PageTableEntry::from_raw(pte_raw);

        match self.level {
            0 => {
                // L0 must be a table descriptor
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 1;
                WalkResult::Continue(next_table + (self.indices[1] as u64) * 8)
            }
            1 => {
                // L1: 1GB block or table
                if pte.is_block() {
                    let offset = self.va & 0x3FFF_FFFF;
                    return WalkResult::Done(pte.output_addr().as_u64() + offset);
                }
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 2;
                WalkResult::Continue(next_table + (self.indices[2] as u64) * 8)
            }
            2 => {
                // L2: 2MB block or table
                if pte.is_block() {
                    let offset = self.va & 0x1F_FFFF;
                    return WalkResult::Done(pte.output_addr().as_u64() + offset);
                }
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 3;
                WalkResult::Continue(next_table + (self.indices[3] as u64) * 8)
            }
            3 => {
                // L3 must be a valid page entry
                if !pte.is_valid() { return WalkResult::Fault; }
                let offset = self.va & 0xFFF;
                WalkResult::Done(pte.output_addr().as_u64() + offset)
            }
            _ => WalkResult::Fault,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_entry_is_invalid() {
        let e = PageTableEntry::empty();
        assert!(!e.is_valid());
        assert!(!e.is_table());
        assert!(!e.is_block());
        assert_eq!(e.raw(), 0);
    }

    #[test]
    fn new_page_sets_all_fields() {
        let e = PageTableEntry::new_page(
            PhysAddr::new(0x4008_0000),
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
        assert!(e.is_valid());
        assert!(e.is_table()); // bit 1 set for L3 page entries
        assert!(e.af());
        assert_eq!(e.attr_index(), MAIR_NORMAL);
        assert_eq!(e.ap(), AP_RW_EL1);
        assert_eq!(e.sh(), SH_INNER);
        assert_eq!(e.output_addr().as_u64(), 0x4008_0000);
    }

    #[test]
    fn new_block_clears_table_bit() {
        let e = PageTableEntry::new_block(
            PhysAddr::new(0x4000_0000),
            MAIR_DEVICE,
            AP_RW_EL1,
            SH_NON,
        );
        assert!(e.is_valid());
        assert!(e.is_block());
        assert!(!e.is_table());
        assert_eq!(e.attr_index(), MAIR_DEVICE);
        assert_eq!(e.sh(), SH_NON);
    }

    #[test]
    fn new_table_descriptor() {
        let e = PageTableEntry::new_table(PhysAddr::new(0x4009_0000));
        assert!(e.is_valid());
        assert!(e.is_table());
        assert!(!e.is_block());
        assert_eq!(e.output_addr().as_u64(), 0x4009_0000);
    }

    #[test]
    fn output_addr_masks_low_bits() {
        let e = PageTableEntry::new_page(
            PhysAddr::new(0x4008_0FFF), // unaligned — low bits should be stripped
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
        assert_eq!(e.output_addr().as_u64(), 0x4008_0000);
    }

    #[test]
    fn with_pxn_sets_bit_53() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x1000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        let e_pxn = e.with_pxn();
        assert_eq!(e_pxn.raw() & (1 << 53), 1 << 53);
        // Original fields unchanged
        assert_eq!(e_pxn.output_addr().as_u64(), 0x1000);
        assert_eq!(e_pxn.ap(), AP_RW_ALL);
    }

    #[test]
    fn with_uxn_sets_bit_54() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x2000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        let e_uxn = e.with_uxn();
        assert_eq!(e_uxn.raw() & (1 << 54), 1 << 54);
    }

    #[test]
    fn ap_rw_all_value() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x1000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        assert_eq!(e.ap(), 0b01);
    }

    #[test]
    fn page_table_empty_all_invalid() {
        let pt = PageTable::empty();
        for i in 0..512 {
            assert!(!pt.entries[i].is_valid());
        }
    }

    #[test]
    fn mair_el1_value_correct() {
        // Attr[0] = 0x00 (device), Attr[1] = 0xFF (normal WB)
        assert_eq!(MAIR_EL1_VALUE, 0xFF00);
    }

    // --- Page table walk tests ---

    #[test]
    fn walk_4_level_resolves() {
        // VA 0x0040_1234: L0=0, L1=0, L2=2, L3=1, offset=0x234
        let va = 0x0040_1234;
        let (mut w, r) = PageTableWalk::start(0x1_0000, va);

        // Should read L0 entry at table + L0_idx*8 = 0x1_0000 + 0*8
        assert_eq!(r, WalkResult::Continue(0x1_0000));

        // L0 → table at 0x2_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x2_0000)); // L1[0]

        // L1 → table at 0x3_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x3_0000 + 2 * 8)); // L2[2]

        // L2 → table at 0x4_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x4_0000 + 1 * 8)); // L3[1]

        // L3 → page at 0x5_0000
        let r = w.step(PageTableEntry::new_page(
            PhysAddr::new(0x5_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert_eq!(r, WalkResult::Done(0x5_0000 + 0x234));
    }

    #[test]
    fn walk_1gb_block_at_l1() {
        // VA 0x4000_1234: L0=0, L1=1, offset within 1GB = 0x1234
        let va = 0x4000_1234;
        let (mut w, r) = PageTableWalk::start(0x1_0000, va);

        // L0 table
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        // Should read L1[1]
        assert_eq!(r, WalkResult::Continue(0x2_0000 + 1 * 8));

        // L1 → 1GB block at 0x8000_0000
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0x8000_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        // offset = 0x4000_1234 & 0x3FFF_FFFF = 0x1234
        assert_eq!(r, WalkResult::Done(0x8000_0000 + 0x1234));
    }

    #[test]
    fn walk_2mb_block_at_l2() {
        // VA 0x0060_5678: L0=0, L1=0, L2=3, offset within 2MB = 0x5678
        let va = 0x0060_5678;
        let (mut w, _) = PageTableWalk::start(0x1_0000, va);

        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        // Should read L2[3]
        assert_eq!(r, WalkResult::Continue(0x3_0000 + 3 * 8));

        // L2 → 2MB block at 0xA00_0000
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0xA00_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        // offset = 0x0060_5678 & 0x1F_FFFF = 0x5678
        assert_eq!(r, WalkResult::Done(0xA00_0000 + 0x5678));
    }

    #[test]
    fn walk_fault_invalid_l0() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l1() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l2() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l3() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_page_offset_preserved() {
        // VA with offset 0xABC within the page
        let va = 0x0040_0ABC;
        let (mut w, _) = PageTableWalk::start(0x1_0000, va);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        let r = w.step(PageTableEntry::new_page(
            PhysAddr::new(0xFF_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert_eq!(r, WalkResult::Done(0xFF_0000 + 0xABC));
    }
}
