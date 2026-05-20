//! PrimeCell PL011 UART (ARM)
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/regspecs/pl011.toml
//! Regenerate with: `cargo xtask gen-regs`.
//! Drift is caught by: `cargo xtask gen-regs --check` (CI).

#![allow(dead_code, missing_docs)]

use lockjaw_mmio::cell::{Ro, Rw, W1c};

/// PrimeCell PL011 UART (ARM)
#[repr(C)]
pub struct Pl011 {
    /// Data register (RX on read, TX on write — independent streams)
    data: Rw<u32>,
    _pad0: [u8; 0x14],
    /// Flag register (RX/TX FIFO status, UART busy)
    flag: Ro<u32>,
    _pad1: [u8; 0x8],
    /// Integer baud rate divisor
    ibrd: Rw<u32>,
    /// Fractional baud rate divisor
    fbrd: Rw<u32>,
    /// Line control
    lcrh: Rw<u32>,
    /// Control register
    control: Rw<u32>,
    _pad2: [u8; 0x4],
    /// Interrupt mask set/clear
    imsc: Rw<u32>,
    _pad3: [u8; 0x8],
    /// Interrupt clear (write 1 to clear)
    icr: W1c<u32>,
}

// ---------- Flag ----------

/// Flag register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Flag(pub u32);

impl Flag {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// UART busy transmitting
    pub const BUSY: Self = Self(1 << 3);
    /// Receive FIFO empty
    pub const RXFE: Self = Self(1 << 4);
    /// Transmit FIFO full
    pub const TXFF: Self = Self(1 << 5);
}

impl core::ops::BitOr for Flag {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for Flag {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for Flag {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}
impl Flag {
    /// Return a copy of `self` with every bit set in `other` also set.
    pub const fn insert(self, other: Self) -> Self { Self(self.0 | other.0) }
    /// Return a copy of `self` with every bit set in `other` cleared.
    pub const fn remove(self, other: Self) -> Self { Self(self.0 & !other.0) }
}

// ---------- Lcrh ----------

/// Lcrh register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Lcrh(pub u32);

impl Lcrh {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// Mask for the `parity_enable` field (bits 1:1).
    pub const PARITY_ENABLE_MASK: u32 = 0x2;
    /// Right-shift to access the `parity_enable` field.
    pub const PARITY_ENABLE_SHIFT: u32 = 1;
    /// Read `parity_enable` as bool.
    pub const fn parity_enable(self) -> bool { (self.0 & Self::PARITY_ENABLE_MASK) != 0 }
    /// Return a new value with `parity_enable` set to `v`.
    pub const fn with_parity_enable(self, v: bool) -> Self {
        if v { Self(self.0 | Self::PARITY_ENABLE_MASK) } else { Self(self.0 & !Self::PARITY_ENABLE_MASK) }
    }
    /// Mask for the `parity_select` field (bits 2:2).
    pub const PARITY_SELECT_MASK: u32 = 0x4;
    /// Right-shift to access the `parity_select` field.
    pub const PARITY_SELECT_SHIFT: u32 = 2;
    /// Decode the `parity_select` field as `LcrhParitySelect`.
    pub const fn parity_select(self) -> Result<LcrhParitySelect, ReservedBits> {
        LcrhParitySelect::from_bits((self.0 & Self::PARITY_SELECT_MASK) >> Self::PARITY_SELECT_SHIFT)
    }
    /// Return a new value with `parity_select` set to `v`.
    pub const fn with_parity_select(self, v: LcrhParitySelect) -> Self {
        Self((self.0 & !Self::PARITY_SELECT_MASK) | ((v.into_bits() as u32) << Self::PARITY_SELECT_SHIFT))
    }
    /// Mask for the `fifo_enable` field (bits 4:4).
    pub const FIFO_ENABLE_MASK: u32 = 0x10;
    /// Right-shift to access the `fifo_enable` field.
    pub const FIFO_ENABLE_SHIFT: u32 = 4;
    /// Read `fifo_enable` as bool.
    pub const fn fifo_enable(self) -> bool { (self.0 & Self::FIFO_ENABLE_MASK) != 0 }
    /// Return a new value with `fifo_enable` set to `v`.
    pub const fn with_fifo_enable(self, v: bool) -> Self {
        if v { Self(self.0 | Self::FIFO_ENABLE_MASK) } else { Self(self.0 & !Self::FIFO_ENABLE_MASK) }
    }
    /// Mask for the `word_length` field (bits 6:5).
    pub const WORD_LENGTH_MASK: u32 = 0x60;
    /// Right-shift to access the `word_length` field.
    pub const WORD_LENGTH_SHIFT: u32 = 5;
    /// Decode the `word_length` field as `LcrhWordLength`.
    pub const fn word_length(self) -> Result<LcrhWordLength, ReservedBits> {
        LcrhWordLength::from_bits((self.0 & Self::WORD_LENGTH_MASK) >> Self::WORD_LENGTH_SHIFT)
    }
    /// Return a new value with `word_length` set to `v`.
    pub const fn with_word_length(self, v: LcrhWordLength) -> Self {
        Self((self.0 & !Self::WORD_LENGTH_MASK) | ((v.into_bits() as u32) << Self::WORD_LENGTH_SHIFT))
    }
}

/// Enum for the `parity_select` field of `Lcrh`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LcrhParitySelect {
    Odd = 0x0,
    Even = 0x1,
}

impl LcrhParitySelect {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u32) -> Result<Self, ReservedBits> {
        match v {
            0x0 => Ok(Self::Odd),
            0x1 => Ok(Self::Even),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u32 { self as u32 }
}

/// Enum for the `word_length` field of `Lcrh`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LcrhWordLength {
    Bits5 = 0x0,
    Bits6 = 0x1,
    Bits7 = 0x2,
    Bits8 = 0x3,
}

impl LcrhWordLength {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u32) -> Result<Self, ReservedBits> {
        match v {
            0x0 => Ok(Self::Bits5),
            0x1 => Ok(Self::Bits6),
            0x2 => Ok(Self::Bits7),
            0x3 => Ok(Self::Bits8),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u32 { self as u32 }
}

// ---------- Control ----------

/// Control register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Control(pub u32);

impl Control {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const UART_ENABLE: Self = Self(1 << 0);
    pub const TX_ENABLE: Self = Self(1 << 8);
    pub const RX_ENABLE: Self = Self(1 << 9);
}

impl core::ops::BitOr for Control {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for Control {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for Control {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}
impl Control {
    /// Return a copy of `self` with every bit set in `other` also set.
    pub const fn insert(self, other: Self) -> Self { Self(self.0 | other.0) }
    /// Return a copy of `self` with every bit set in `other` cleared.
    pub const fn remove(self, other: Self) -> Self { Self(self.0 & !other.0) }
}

// ---------- Imsc ----------

/// Imsc register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Imsc(pub u32);

impl Imsc {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// RX interrupt mask
    pub const RXIM: Self = Self(1 << 4);
    /// TX interrupt mask
    pub const TXIM: Self = Self(1 << 5);
}

impl core::ops::BitOr for Imsc {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for Imsc {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for Imsc {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}
impl Imsc {
    /// Return a copy of `self` with every bit set in `other` also set.
    pub const fn insert(self, other: Self) -> Self { Self(self.0 | other.0) }
    /// Return a copy of `self` with every bit set in `other` cleared.
    pub const fn remove(self, other: Self) -> Self { Self(self.0 & !other.0) }
}

// ---------- Icr ----------

/// Icr register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Icr(pub u32);

impl Icr {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const RXIC: Self = Self(1 << 4);
    pub const TXIC: Self = Self(1 << 5);
}

impl core::ops::BitOr for Icr {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for Icr {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for Icr {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}
impl Icr {
    /// Return a copy of `self` with every bit set in `other` also set.
    pub const fn insert(self, other: Self) -> Self { Self(self.0 | other.0) }
    /// Return a copy of `self` with every bit set in `other` cleared.
    pub const fn remove(self, other: Self) -> Self { Self(self.0 & !other.0) }
}

/// Returned when an enum decode sees a bit pattern that does not
/// correspond to any declared variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReservedBits(pub u64);

// ---------- Pl011 accessors ----------

impl Pl011 {
    /// Volatile read of `data` as `u32`.
    #[inline(always)]
    pub fn read_data(&self) -> u32 { self.data.read() }
    /// Volatile write of `data`.
    #[inline(always)]
    pub fn write_data(&self, v: u32) { self.data.write(v); }
    /// Read a typed snapshot of `flag`.
    #[inline(always)]
    pub fn flag(&self) -> Flag { Flag(self.flag.read()) }
    /// Volatile read of `ibrd` as `u32`.
    #[inline(always)]
    pub fn read_ibrd(&self) -> u32 { self.ibrd.read() }
    /// Volatile write of `ibrd`.
    #[inline(always)]
    pub fn write_ibrd(&self, v: u32) { self.ibrd.write(v); }
    /// Read-modify-write `ibrd`.
    #[inline(always)]
    pub fn modify_ibrd<F: FnOnce(u32) -> u32>(&self, f: F) {
        self.ibrd.modify(f);
    }
    /// Volatile read of `fbrd` as `u32`.
    #[inline(always)]
    pub fn read_fbrd(&self) -> u32 { self.fbrd.read() }
    /// Volatile write of `fbrd`.
    #[inline(always)]
    pub fn write_fbrd(&self, v: u32) { self.fbrd.write(v); }
    /// Read-modify-write `fbrd`.
    #[inline(always)]
    pub fn modify_fbrd<F: FnOnce(u32) -> u32>(&self, f: F) {
        self.fbrd.modify(f);
    }
    /// Read a typed snapshot of `lcrh`.
    #[inline(always)]
    pub fn lcrh(&self) -> Lcrh { Lcrh(self.lcrh.read()) }
    /// Write the value back to `lcrh`.
    #[inline(always)]
    pub fn set_lcrh(&self, v: Lcrh) { self.lcrh.write(v.0); }
    /// Read-modify-write `lcrh` via a typed closure.
    #[inline(always)]
    pub fn modify_lcrh<F: FnOnce(Lcrh) -> Lcrh>(&self, f: F) {
        self.lcrh.modify(|v| f(Lcrh(v)).0);
    }
    /// Read a typed snapshot of `control`.
    #[inline(always)]
    pub fn control(&self) -> Control { Control(self.control.read()) }
    /// Write the value back to `control`.
    #[inline(always)]
    pub fn set_control(&self, v: Control) { self.control.write(v.0); }
    /// Read-modify-write `control` via a typed closure.
    #[inline(always)]
    pub fn modify_control<F: FnOnce(Control) -> Control>(&self, f: F) {
        self.control.modify(|v| f(Control(v)).0);
    }
    /// Read a typed snapshot of `imsc`.
    #[inline(always)]
    pub fn imsc(&self) -> Imsc { Imsc(self.imsc.read()) }
    /// Write the value back to `imsc`.
    #[inline(always)]
    pub fn set_imsc(&self, v: Imsc) { self.imsc.write(v.0); }
    /// Read-modify-write `imsc` via a typed closure.
    #[inline(always)]
    pub fn modify_imsc<F: FnOnce(Imsc) -> Imsc>(&self, f: F) {
        self.imsc.modify(|v| f(Imsc(v)).0);
    }
    /// Clear bits in `icr` (write-1-to-clear).
    #[inline(always)]
    pub fn clear_icr(&self, mask: Icr) { self.icr.clear(mask.0); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn layout_offsets() {
        assert_eq!(offset_of!(Pl011, data), 0x0, "data offset");
        assert_eq!(offset_of!(Pl011, flag), 0x18, "flag offset");
        assert_eq!(offset_of!(Pl011, ibrd), 0x24, "ibrd offset");
        assert_eq!(offset_of!(Pl011, fbrd), 0x28, "fbrd offset");
        assert_eq!(offset_of!(Pl011, lcrh), 0x2c, "lcrh offset");
        assert_eq!(offset_of!(Pl011, control), 0x30, "control offset");
        assert_eq!(offset_of!(Pl011, imsc), 0x38, "imsc offset");
        assert_eq!(offset_of!(Pl011, icr), 0x44, "icr offset");
    }

    #[test]
    fn flag_flag_bits() {
        assert_eq!(Flag::BUSY.bits(), 1 << 3, "busy");
        assert_eq!(Flag::RXFE.bits(), 1 << 4, "rxfe");
        assert_eq!(Flag::TXFF.bits(), 1 << 5, "txff");
    }

    #[test]
    fn flag_flag_compose() {
        let v = Flag::BUSY | Flag::RXFE;
        assert!(v.contains(Flag::BUSY));
        assert!(v.contains(Flag::RXFE));
        assert!(!Flag::BUSY.contains(Flag::RXFE));
    }

    #[test]
    fn control_flag_bits() {
        assert_eq!(Control::UART_ENABLE.bits(), 1 << 0, "uart_enable");
        assert_eq!(Control::TX_ENABLE.bits(), 1 << 8, "tx_enable");
        assert_eq!(Control::RX_ENABLE.bits(), 1 << 9, "rx_enable");
    }

    #[test]
    fn control_flag_compose() {
        let v = Control::UART_ENABLE | Control::TX_ENABLE;
        assert!(v.contains(Control::UART_ENABLE));
        assert!(v.contains(Control::TX_ENABLE));
        assert!(!Control::UART_ENABLE.contains(Control::TX_ENABLE));
    }

    #[test]
    fn imsc_flag_bits() {
        assert_eq!(Imsc::RXIM.bits(), 1 << 4, "rxim");
        assert_eq!(Imsc::TXIM.bits(), 1 << 5, "txim");
    }

    #[test]
    fn imsc_flag_compose() {
        let v = Imsc::RXIM | Imsc::TXIM;
        assert!(v.contains(Imsc::RXIM));
        assert!(v.contains(Imsc::TXIM));
        assert!(!Imsc::RXIM.contains(Imsc::TXIM));
    }

    #[test]
    fn icr_flag_bits() {
        assert_eq!(Icr::RXIC.bits(), 1 << 4, "rxic");
        assert_eq!(Icr::TXIC.bits(), 1 << 5, "txic");
    }

    #[test]
    fn icr_flag_compose() {
        let v = Icr::RXIC | Icr::TXIC;
        assert!(v.contains(Icr::RXIC));
        assert!(v.contains(Icr::TXIC));
        assert!(!Icr::RXIC.contains(Icr::TXIC));
    }

    #[test]
    fn lcrh_field_roundtrip() {
        // bool field `parity_enable`
        let on = Lcrh::default().with_parity_enable(true);
        assert!(on.parity_enable());
        let off = on.with_parity_enable(false);
        assert!(!off.parity_enable());
        // enum field `parity_select`
        assert_eq!(Lcrh::default().with_parity_select(LcrhParitySelect::Odd).parity_select().unwrap(), LcrhParitySelect::Odd);
        assert_eq!(Lcrh::default().with_parity_select(LcrhParitySelect::Even).parity_select().unwrap(), LcrhParitySelect::Even);
        // bool field `fifo_enable`
        let on = Lcrh::default().with_fifo_enable(true);
        assert!(on.fifo_enable());
        let off = on.with_fifo_enable(false);
        assert!(!off.fifo_enable());
        // enum field `word_length`
        assert_eq!(Lcrh::default().with_word_length(LcrhWordLength::Bits5).word_length().unwrap(), LcrhWordLength::Bits5);
        assert_eq!(Lcrh::default().with_word_length(LcrhWordLength::Bits6).word_length().unwrap(), LcrhWordLength::Bits6);
        assert_eq!(Lcrh::default().with_word_length(LcrhWordLength::Bits7).word_length().unwrap(), LcrhWordLength::Bits7);
        assert_eq!(Lcrh::default().with_word_length(LcrhWordLength::Bits8).word_length().unwrap(), LcrhWordLength::Bits8);
    }

    #[test]
    fn lcrh_preserves_reserved_bits() {
        let all_ones: u32 = !0;
        // toggle bool `parity_enable` without disturbing other bits
        let v = Lcrh(all_ones).with_parity_enable(false);
        assert_eq!(v.0 & !Lcrh::PARITY_ENABLE_MASK, all_ones & !Lcrh::PARITY_ENABLE_MASK);
        // set enum `parity_select` to a known variant
        let v = Lcrh(all_ones).with_parity_select(LcrhParitySelect::Odd);
        assert_eq!(v.0 & !Lcrh::PARITY_SELECT_MASK, all_ones & !Lcrh::PARITY_SELECT_MASK);
        // toggle bool `fifo_enable` without disturbing other bits
        let v = Lcrh(all_ones).with_fifo_enable(false);
        assert_eq!(v.0 & !Lcrh::FIFO_ENABLE_MASK, all_ones & !Lcrh::FIFO_ENABLE_MASK);
        // set enum `word_length` to a known variant
        let v = Lcrh(all_ones).with_word_length(LcrhWordLength::Bits5);
        assert_eq!(v.0 & !Lcrh::WORD_LENGTH_MASK, all_ones & !Lcrh::WORD_LENGTH_MASK);
    }

    #[test]
    fn lcrh_parity_select_enum_decode() {
        assert_eq!(LcrhParitySelect::from_bits(0x0), Ok(LcrhParitySelect::Odd));
        assert_eq!(LcrhParitySelect::from_bits(0x1), Ok(LcrhParitySelect::Even));
    }

    #[test]
    fn lcrh_word_length_enum_decode() {
        assert_eq!(LcrhWordLength::from_bits(0x0), Ok(LcrhWordLength::Bits5));
        assert_eq!(LcrhWordLength::from_bits(0x1), Ok(LcrhWordLength::Bits6));
        assert_eq!(LcrhWordLength::from_bits(0x2), Ok(LcrhWordLength::Bits7));
        assert_eq!(LcrhWordLength::from_bits(0x3), Ok(LcrhWordLength::Bits8));
    }

}
