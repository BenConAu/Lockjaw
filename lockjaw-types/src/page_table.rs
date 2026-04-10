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
}
