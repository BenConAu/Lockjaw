//! BCM2711 Clock & Power Manager (Pi 4B clock controller)
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/regspecs/cprman.toml
//! Regenerate with: `cargo xtask gen-regs`.
//! Drift is caught by: `cargo xtask gen-regs --check` (CI).

#![allow(dead_code, missing_docs)]

use lockjaw_mmio::cell::{Rw};

//
// verify_against: lockjaw_types::clock::cprman
// Coverage: 2/2 registers cross-checked against constants.
//

/// BCM2711 Clock & Power Manager (Pi 4B clock controller)
#[repr(C)]
pub struct Cprman {
    _pad0: [u8; 0x1d0],
    /// EMMC2 clock-gate control. PASSWD-protected.
    cm_emmc2_ctl: Rw<u32>,
    /// EMMC2 clock integer+fractional divider (DIVI<<12 | DIVF). PASSWD-protected.
    cm_emmc2_div: Rw<u32>,
}

// ---------- CmEmmc2Ctl ----------

/// CmEmmc2Ctl register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct CmEmmc2Ctl(pub u32);

impl CmEmmc2Ctl {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// Mask for the `src` field (bits 3:0).
    pub const SRC_MASK: u32 = 0xf;
    /// Right-shift to access the `src` field.
    pub const SRC_SHIFT: u32 = 0;
    /// Decode the `src` field as `CmEmmc2CtlSrc`.
    pub const fn src(self) -> Result<CmEmmc2CtlSrc, ReservedBits> {
        CmEmmc2CtlSrc::from_bits((self.0 & Self::SRC_MASK) >> Self::SRC_SHIFT)
    }
    /// Return a new value with `src` set to `v`.
    pub const fn with_src(self, v: CmEmmc2CtlSrc) -> Self {
        Self((self.0 & !Self::SRC_MASK) | ((v.into_bits() as u32) << Self::SRC_SHIFT))
    }
    /// Mask for the `enable` field (bits 4:4).
    pub const ENABLE_MASK: u32 = 0x10;
    /// Right-shift to access the `enable` field.
    pub const ENABLE_SHIFT: u32 = 4;
    /// Read `enable` as bool.
    pub const fn enable(self) -> bool { (self.0 & Self::ENABLE_MASK) != 0 }
    /// Return a new value with `enable` set to `v`.
    pub const fn with_enable(self, v: bool) -> Self {
        if v { Self(self.0 | Self::ENABLE_MASK) } else { Self(self.0 & !Self::ENABLE_MASK) }
    }
    /// Mask for the `kill` field (bits 5:5).
    pub const KILL_MASK: u32 = 0x20;
    /// Right-shift to access the `kill` field.
    pub const KILL_SHIFT: u32 = 5;
    /// Read `kill` as bool.
    pub const fn kill(self) -> bool { (self.0 & Self::KILL_MASK) != 0 }
    /// Return a new value with `kill` set to `v`.
    pub const fn with_kill(self, v: bool) -> Self {
        if v { Self(self.0 | Self::KILL_MASK) } else { Self(self.0 & !Self::KILL_MASK) }
    }
    /// Mask for the `busy` field (bits 7:7).
    pub const BUSY_MASK: u32 = 0x80;
    /// Right-shift to access the `busy` field.
    pub const BUSY_SHIFT: u32 = 7;
    /// Read `busy` as bool.
    pub const fn busy(self) -> bool { (self.0 & Self::BUSY_MASK) != 0 }
}

/// Enum for the `src` field of `CmEmmc2Ctl`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CmEmmc2CtlSrc {
    Gnd = 0x0,
    Osc = 0x1,
    PllDPerCore = 0x6,
}

impl CmEmmc2CtlSrc {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u32) -> Result<Self, ReservedBits> {
        match v {
            0x0 => Ok(Self::Gnd),
            0x1 => Ok(Self::Osc),
            0x6 => Ok(Self::PllDPerCore),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u32 { self as u32 }
}

// ---------- CmEmmc2Div ----------

/// CmEmmc2Div register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct CmEmmc2Div(pub u32);

impl CmEmmc2Div {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// Mask for the `divf` field (bits 11:0).
    pub const DIVF_MASK: u32 = 0xfff;
    /// Right-shift to access the `divf` field.
    pub const DIVF_SHIFT: u32 = 0;
    /// Read the `divf` field as a scalar.
    pub const fn divf(self) -> u32 { (self.0 & Self::DIVF_MASK) >> Self::DIVF_SHIFT }
    /// Return a new value with `divf` set to `v` (truncated to field width).
    pub const fn with_divf(self, v: u32) -> Self {
        Self((self.0 & !Self::DIVF_MASK) | ((v << Self::DIVF_SHIFT) & Self::DIVF_MASK))
    }
    /// Mask for the `divi` field (bits 23:12).
    pub const DIVI_MASK: u32 = 0xfff000;
    /// Right-shift to access the `divi` field.
    pub const DIVI_SHIFT: u32 = 12;
    /// Read the `divi` field as a scalar.
    pub const fn divi(self) -> u32 { (self.0 & Self::DIVI_MASK) >> Self::DIVI_SHIFT }
    /// Return a new value with `divi` set to `v` (truncated to field width).
    pub const fn with_divi(self, v: u32) -> Self {
        Self((self.0 & !Self::DIVI_MASK) | ((v << Self::DIVI_SHIFT) & Self::DIVI_MASK))
    }
}

/// Returned when an enum decode sees a bit pattern that does not
/// correspond to any declared variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReservedBits(pub u64);

// ---------- Cprman accessors ----------

impl Cprman {
    /// Read a typed snapshot of `cm_emmc2_ctl`.
    #[inline(always)]
    pub fn cm_emmc2_ctl(&self) -> CmEmmc2Ctl { CmEmmc2Ctl(self.cm_emmc2_ctl.read()) }
    /// Write the value back to `cm_emmc2_ctl` (PASSWD prefix injected automatically).
    #[inline(always)]
    pub fn set_cm_emmc2_ctl(&self, v: CmEmmc2Ctl) { self.cm_emmc2_ctl.write(((v.0) & 0x00FF_FFFFu32) | (0x5Au32 << 24)); }
    /// Read-modify-write `cm_emmc2_ctl` via a typed closure (PASSWD prefix injected automatically).
    #[inline(always)]
    pub fn modify_cm_emmc2_ctl<F: FnOnce(CmEmmc2Ctl) -> CmEmmc2Ctl>(&self, f: F) {
        self.cm_emmc2_ctl.modify(|v| ((f(CmEmmc2Ctl(v)).0) & 0x00FF_FFFFu32) | (0x5Au32 << 24));
    }
    /// Read a typed snapshot of `cm_emmc2_div`.
    #[inline(always)]
    pub fn cm_emmc2_div(&self) -> CmEmmc2Div { CmEmmc2Div(self.cm_emmc2_div.read()) }
    /// Write the value back to `cm_emmc2_div` (PASSWD prefix injected automatically).
    #[inline(always)]
    pub fn set_cm_emmc2_div(&self, v: CmEmmc2Div) { self.cm_emmc2_div.write(((v.0) & 0x00FF_FFFFu32) | (0x5Au32 << 24)); }
    /// Read-modify-write `cm_emmc2_div` via a typed closure (PASSWD prefix injected automatically).
    #[inline(always)]
    pub fn modify_cm_emmc2_div<F: FnOnce(CmEmmc2Div) -> CmEmmc2Div>(&self, f: F) {
        self.cm_emmc2_div.modify(|v| ((f(CmEmmc2Div(v)).0) & 0x00FF_FFFFu32) | (0x5Au32 << 24));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use lockjaw_mmio::mock::MockMmioRegion;

    #[test]
    fn layout_offsets() {
        assert_eq!(offset_of!(Cprman, cm_emmc2_ctl), 0x1d0, "cm_emmc2_ctl offset");
        assert_eq!(offset_of!(Cprman, cm_emmc2_div), 0x1d4, "cm_emmc2_div offset");
    }

    #[test]
    fn cm_emmc2_ctl_field_roundtrip() {
        // enum field `src`
        assert_eq!(CmEmmc2Ctl::default().with_src(CmEmmc2CtlSrc::Gnd).src().unwrap(), CmEmmc2CtlSrc::Gnd);
        assert_eq!(CmEmmc2Ctl::default().with_src(CmEmmc2CtlSrc::Osc).src().unwrap(), CmEmmc2CtlSrc::Osc);
        assert_eq!(CmEmmc2Ctl::default().with_src(CmEmmc2CtlSrc::PllDPerCore).src().unwrap(), CmEmmc2CtlSrc::PllDPerCore);
        // bool field `enable`
        let on = CmEmmc2Ctl::default().with_enable(true);
        assert!(on.enable());
        let off = on.with_enable(false);
        assert!(!off.enable());
        // bool field `kill`
        let on = CmEmmc2Ctl::default().with_kill(true);
        assert!(on.kill());
        let off = on.with_kill(false);
        assert!(!off.kill());
        // RO field `busy` — see cm_emmc2_ctl_ro_field_read
    }

    #[test]
    fn cm_emmc2_ctl_preserves_reserved_bits() {
        let all_ones: u32 = !0;
        // set enum `src` to a known variant
        let v = CmEmmc2Ctl(all_ones).with_src(CmEmmc2CtlSrc::Gnd);
        assert_eq!(v.0 & !CmEmmc2Ctl::SRC_MASK, all_ones & !CmEmmc2Ctl::SRC_MASK);
        // toggle bool `enable` without disturbing other bits
        let v = CmEmmc2Ctl(all_ones).with_enable(false);
        assert_eq!(v.0 & !CmEmmc2Ctl::ENABLE_MASK, all_ones & !CmEmmc2Ctl::ENABLE_MASK);
        // toggle bool `kill` without disturbing other bits
        let v = CmEmmc2Ctl(all_ones).with_kill(false);
        assert_eq!(v.0 & !CmEmmc2Ctl::KILL_MASK, all_ones & !CmEmmc2Ctl::KILL_MASK);
    }

    #[test]
    fn cm_emmc2_ctl_ro_field_read() {
        // RO bool `busy` — bit 7 reads as the field state
        assert!(CmEmmc2Ctl(0x80u32).busy());
        assert!(!CmEmmc2Ctl(!0x80u32).busy());
    }

    #[test]
    fn cm_emmc2_ctl_src_enum_decode() {
        assert_eq!(CmEmmc2CtlSrc::from_bits(0x0), Ok(CmEmmc2CtlSrc::Gnd));
        assert_eq!(CmEmmc2CtlSrc::from_bits(0x1), Ok(CmEmmc2CtlSrc::Osc));
        assert_eq!(CmEmmc2CtlSrc::from_bits(0x6), Ok(CmEmmc2CtlSrc::PllDPerCore));
        assert_eq!(CmEmmc2CtlSrc::from_bits(0x2), Err(ReservedBits(0x2)));
    }

    #[test]
    fn cm_emmc2_div_field_roundtrip() {
        // scalar field `divf`
        for v in [0 as u32, 1, 0xfff] {
            assert_eq!(CmEmmc2Div::default().with_divf(v).divf(), v);
        }
        // scalar field `divi`
        for v in [0 as u32, 1, 0xfff] {
            assert_eq!(CmEmmc2Div::default().with_divi(v).divi(), v);
        }
    }

    #[test]
    fn cm_emmc2_div_preserves_reserved_bits() {
        let all_ones: u32 = !0;
        // set scalar `divf` to 0
        let v = CmEmmc2Div(all_ones).with_divf(0);
        assert_eq!(v.0 & !CmEmmc2Div::DIVF_MASK, all_ones & !CmEmmc2Div::DIVF_MASK);
        // set scalar `divi` to 0
        let v = CmEmmc2Div(all_ones).with_divi(0);
        assert_eq!(v.0 & !CmEmmc2Div::DIVI_MASK, all_ones & !CmEmmc2Div::DIVI_MASK);
    }

    #[test]
    fn cm_emmc2_ctl_passwd_wrap_zero_payload() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        regs.regs().set_cm_emmc2_ctl(CmEmmc2Ctl(0u32));
        // PASSWD (0x5A) must occupy bits[31:24]; low 24 bits stay 0.
        assert_eq!(region.peek_u32(0x1d0), 0x5A00_0000u32);
    }

    #[test]
    fn cm_emmc2_ctl_passwd_wrap_preserves_payload() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        regs.regs().set_cm_emmc2_ctl(CmEmmc2Ctl(0x00AB_CDEFu32));
        // PASSWD prefix AND the 24-bit payload survive intact.
        assert_eq!(region.peek_u32(0x1d0), 0x5AAB_CDEFu32);
    }

    #[test]
    fn cm_emmc2_ctl_passwd_wrap_in_modify() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        // Seed memory with a payload — modify_ must keep low bits, replace top.
        region.poke_u32(0x1d0, 0x0000_00FFu32);
        regs.regs().modify_cm_emmc2_ctl(|v| CmEmmc2Ctl(v.0 | 0xFF00u32));
        // Closure produced 0x0000_FFFFu32; codegen OR-ed PASSWD on top.
        assert_eq!(region.peek_u32(0x1d0), 0x5A00_FFFFu32);
    }

    #[test]
    fn cm_emmc2_div_passwd_wrap_zero_payload() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        regs.regs().set_cm_emmc2_div(CmEmmc2Div(0u32));
        // PASSWD (0x5A) must occupy bits[31:24]; low 24 bits stay 0.
        assert_eq!(region.peek_u32(0x1d4), 0x5A00_0000u32);
    }

    #[test]
    fn cm_emmc2_div_passwd_wrap_preserves_payload() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        regs.regs().set_cm_emmc2_div(CmEmmc2Div(0x00AB_CDEFu32));
        // PASSWD prefix AND the 24-bit payload survive intact.
        assert_eq!(region.peek_u32(0x1d4), 0x5AAB_CDEFu32);
    }

    #[test]
    fn cm_emmc2_div_passwd_wrap_in_modify() {
        let region = MockMmioRegion::for_layout::<Cprman>();
        let regs = region.as_mapped_regs::<Cprman>();
        // Seed memory with a payload — modify_ must keep low bits, replace top.
        region.poke_u32(0x1d4, 0x0000_00FFu32);
        regs.regs().modify_cm_emmc2_div(|v| CmEmmc2Div(v.0 | 0xFF00u32));
        // Closure produced 0x0000_FFFFu32; codegen OR-ed PASSWD on top.
        assert_eq!(region.peek_u32(0x1d4), 0x5A00_FFFFu32);
    }

    mod _verify {
        use super::*;
        use static_assertions::const_assert_eq;
        const_assert_eq!(
            offset_of!(Cprman, cm_emmc2_ctl) as u64,
            lockjaw_types::clock::cprman::CM_EMMC2CTL
        );
        const_assert_eq!(
            offset_of!(Cprman, cm_emmc2_div) as u64,
            lockjaw_types::clock::cprman::CM_EMMC2DIV
        );
    }

}
