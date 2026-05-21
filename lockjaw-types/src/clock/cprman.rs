/// BCM2711 CPRMAN clock-controller specifics.
///
/// The Pi 4B's clock manager is undocumented in the BCM2711 ARM
/// Peripherals manual; the canonical reference is Linux's
/// `drivers/clk/bcm/clk-bcm2835.c` plus the upstream DT binding
/// `include/dt-bindings/clock/bcm2835.h`. The constants below match
/// that binding so a `clocks = <&cprman BCM2835_CLOCK_EMMC2>`
/// reference parsed from the DTB resolves to the same id Linux
/// would name.
///
/// Scope this milestone (M0b): CM_EMMC2 only. Every other ClockId
/// returns `Err(ClockError::NotSupported)` — see the M0b plan and
/// the README's emmc2 section. The infrastructure (PLL parents,
/// register layout, divider math) is the final shape; new leaves
/// are register-offset additions, not abstraction redesigns.

use super::ClockError;

/// Per-controller clock identifier. Values match
/// `include/dt-bindings/clock/bcm2835.h` so DTB-resolved
/// `clock_id` integers feed straight into this enum via `try_from`.
///
/// `#[repr(u32)]` so the discriminant matches the on-the-wire id
/// in the IPC message word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ClockId {
    /// CM_EMMC2 — the SDHCI controller behind the Pi 4B's microSD
    /// slot. The only leaf implemented this milestone.
    Emmc2 = 51,
}

impl ClockId {
    /// Decode the on-the-wire `clock_id` from a DTB clocks property
    /// or an IPC message into the typed enum. Unknown ids return
    /// `NotSupported(id)` — keeps the supported set explicit and
    /// the caller's logging meaningful.
    pub fn try_from_u32(id: u32) -> Result<Self, ClockError> {
        match id {
            51 => Ok(ClockId::Emmc2),
            other => Err(ClockError::NotSupported(other)),
        }
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

// ---------------------------------------------------------------------------
// CPRMAN MMIO register layout
// ---------------------------------------------------------------------------
//
// The DTB declares the CPRMAN region as 0x2000 bytes (8 KB / 2 pages);
// drivers today map only the first page because the device-manager
// claim path is one-page (`sys_register_device_page`). Every register
// the M0b implementation touches (CM_EMMC2CTL, CM_EMMC2DIV) is below
// 0x1000. When a future leaf needs registers in the second page,
// extend the claim path first.

/// Password protecting every CM_/A2W_ register write. Without
/// `(value & 0x00FF_FFFF) | (CM_PASSWORD << 24)` the hardware
/// silently ignores the write. See clk-bcm2835.c:CM_PASSWORD.
pub const CM_PASSWORD: u32 = 0x5A;

/// CM_EMMC2CTL register offset (Linux: BCM2835_REG_CM_EMMC2CTL).
/// Bits: 7 BUSY (RO), 5 KILL, 4 ENAB (gate), 3:0 SRC (parent select).
///
/// `u64` so the regspec codegen's `verify_against` const_assert
/// (which casts `offset_of` to u64) compiles cleanly. The constant
/// is the source of truth that the generated module's register
/// offset must match.
pub const CM_EMMC2CTL: u64 = 0x1d0;

/// CM_EMMC2DIV register offset. 24-bit fixed-point divider:
/// bits[23:12] = integer part (DIVI), bits[11:0] = fractional
/// part (DIVF, in /4096 units). Output rate = parent / divider.
pub const CM_EMMC2DIV: u64 = 0x1d4;

/// CTL bit positions.
pub const CM_CTL_BUSY: u32 = 1 << 7;
pub const CM_CTL_ENABLE: u32 = 1 << 4;
pub const CM_CTL_KILL: u32 = 1 << 5;
/// SRC field: parent-clock selector (4 bits). For EMMC2 on Pi 4B
/// this is PLLD_PER_CORE = 6 per the binding.
pub const CM_CTL_SRC_SHIFT: u32 = 0;
pub const CM_CTL_SRC_MASK: u32 = 0xf;
pub const CM_SRC_PLLD_PER_CORE: u32 = 6;

// ---------------------------------------------------------------------------
// Divider math (pure, host-tested)
// ---------------------------------------------------------------------------

/// Compute the 24-bit fixed-point divider for a target rate against
/// a parent rate. Returns the encoded divider value (DIVI in
/// bits[23:12], DIVF in bits[11:0]) plus the actual rate that
/// divider produces.
///
/// The divider value is `parent_hz * 4096 / target_hz` (rounded to
/// nearest, clamped to the 24-bit range). The actual output rate is
/// `parent_hz * 4096 / divider`.
///
/// Returns `OutOfRange` if the target rate would require a divider
/// outside `[0x1000, 0xFFFFFF]` (i.e., target > parent or target
/// less than `parent / 4095`).
pub fn compute_divider(parent_hz: u64, target_hz: u64) -> Result<(u32, u64), ClockError> {
    if target_hz == 0 || parent_hz == 0 {
        return Err(ClockError::OutOfRange);
    }
    // Round-to-nearest division.
    let div = (parent_hz.saturating_mul(4096) + (target_hz / 2)) / target_hz;
    if div < 0x1000 || div > 0xFF_FFFF {
        return Err(ClockError::OutOfRange);
    }
    let divider = div as u32;
    let actual_hz = (parent_hz * 4096 + (divider as u64 / 2)) / (divider as u64);
    Ok((divider, actual_hz))
}

/// Pi 4B PLLD_PER_CORE rate (the parent of CM_EMMC2). The VC
/// firmware programs PLLD_PER to 750 MHz at boot; the per-core
/// divider isn't touched by us this milestone, so the EMMC2
/// parent is exactly 750 MHz.
///
/// A future enhancement would read this from the PLL registers
/// rather than hardcoding it; for M0b's "drive emmc2 to 200 MHz"
/// goal the value is fixed by the boot configuration anyway.
pub const PLLD_PER_CORE_HZ: u64 = 750_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_target_equals_parent() {
        // 1.0 divider = output equals parent.
        let (div, actual) = compute_divider(750_000_000, 750_000_000).unwrap();
        assert_eq!(div, 0x1000);
        assert_eq!(actual, 750_000_000);
    }

    #[test]
    fn divider_target_half_parent() {
        // 2.0 divider = 1/2 the parent.
        let (div, actual) = compute_divider(750_000_000, 375_000_000).unwrap();
        assert_eq!(div, 0x2000);
        assert_eq!(actual, 375_000_000);
    }

    #[test]
    fn divider_emmc2_at_200mhz_from_plld_per() {
        // The M0b success-line target. 750 MHz / 200 MHz = 3.75 →
        // divider = 3.75 * 4096 = 15360 = 0x3C00. DIVI = 3, DIVF = 0xC00.
        let (div, actual) = compute_divider(PLLD_PER_CORE_HZ, 200_000_000).unwrap();
        assert_eq!(div, 0x3C00, "divider for 200 MHz from 750 MHz PLLD_PER");
        assert_eq!(div >> 12, 3, "DIVI (integer part) = 3");
        assert_eq!(div & 0xFFF, 0xC00, "DIVF (fractional part) = 0xC00 (0.75)");
        assert_eq!(actual, 200_000_000, "actual matches target exactly");
    }

    #[test]
    fn divider_400khz_id_mode() {
        // SD ID-mode rate. 750 MHz / 400 kHz = 1875.0 → divider =
        // 1875 * 4096 = 7,680,000 = 0x753000. DIVI = 0x753, DIVF = 0.
        let (div, actual) = compute_divider(PLLD_PER_CORE_HZ, 400_000).unwrap();
        assert_eq!(div >> 12, 0x753);
        assert_eq!(div & 0xFFF, 0);
        assert_eq!(actual, 400_000);
    }

    #[test]
    fn divider_out_of_range_target_above_parent() {
        // target > parent → divider < 1.0 → out of range.
        let r = compute_divider(750_000_000, 1_000_000_000);
        assert_eq!(r.unwrap_err(), ClockError::OutOfRange);
    }

    #[test]
    fn divider_out_of_range_target_too_low() {
        // target < parent / 4095 → divider > 0xFFFFFF → out of range.
        // 750e6 / 4095 ≈ 183_150 Hz; ask for 1 Hz.
        let r = compute_divider(750_000_000, 1);
        assert_eq!(r.unwrap_err(), ClockError::OutOfRange);
    }

    #[test]
    fn divider_zero_target_is_error() {
        assert_eq!(compute_divider(750_000_000, 0).unwrap_err(),
                   ClockError::OutOfRange);
    }

    #[test]
    fn divider_zero_parent_is_error() {
        assert_eq!(compute_divider(0, 200_000_000).unwrap_err(),
                   ClockError::OutOfRange);
    }

    #[test]
    fn clock_id_decode_emmc2() {
        assert_eq!(ClockId::try_from_u32(51).unwrap(), ClockId::Emmc2);
        assert_eq!(ClockId::Emmc2.as_u32(), 51);
    }

    #[test]
    fn clock_id_decode_unknown_yields_not_supported() {
        let r = ClockId::try_from_u32(99);
        assert_eq!(r.unwrap_err(), ClockError::NotSupported(99));
        // UART (id 19 in the BCM2711 binding) is not implemented this
        // milestone — should also surface as NotSupported.
        let r = ClockId::try_from_u32(19);
        assert_eq!(r.unwrap_err(), ClockError::NotSupported(19));
    }
}
