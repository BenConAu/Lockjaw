//! SD Host Controller v3 / BCM2711 emmc2
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/regspecs/sdhci.toml
//! Regenerate with: `cargo xtask gen-regs`.
//! Drift is caught by: `cargo xtask gen-regs --check` (CI).

#![allow(dead_code, missing_docs)]

use lockjaw_mmio::cell::{Ro, Rw, W1c, Wo};

//
// verify_against: lockjaw_types::sdhci
// Coverage: 28/28 registers cross-checked against constants.
//

/// SD Host Controller v3 / BCM2711 emmc2
#[repr(C)]
pub struct Sdhci {
    /// SDMA system address / ARGUMENT2 alias (Auto-CMD23 block count)
    sysaddr: Rw<u32>,
    /// Block size in bytes (bits[11:0] = size; max 4 KiB per spec)
    block_size: Rw<u16>,
    /// Block count for the current transfer
    block_count: Rw<u16>,
    /// Command argument (sector index for CMD17, OCR for ACMD41, etc.)
    argument: Rw<u32>,
    /// TRANSFER_MODE (low u16) + COMMAND (high u16) — single u32 store triggers the command
    transfer_mode_command: Wo<u32>,
    /// Response slot 0 (short response payload; long-response low 32)
    response_0: Ro<u32>,
    /// Response slot 1 (long-response bits 63:32)
    response_1: Ro<u32>,
    /// Response slot 2 (long-response bits 95:64)
    response_2: Ro<u32>,
    /// Response slot 3 (long-response bits 127:96)
    response_3: Ro<u32>,
    /// PIO data window (unused — Lockjaw uses ADMA2 for all data transfers)
    buffer_data_port: Rw<u32>,
    /// Controller status snapshot (CMD/DAT inhibits, card detect, transfer in progress)
    present_state: Ro<u32>,
    /// Host Control 1 (bus width + DMA engine select)
    host_control: Rw<u8>,
    /// Power Control (bus power enable + voltage select)
    power_control: Rw<u8>,
    /// Block Gap Control (interrupt-at-block-gap and continue request — unused by emmc2)
    block_gap_control: Rw<u8>,
    /// Wakeup Control (card insertion / removal / interrupt wake events — unused by emmc2)
    wakeup_control: Rw<u8>,
    /// Clock Control (internal/SD clock enables + frequency divisor)
    clock_control: Rw<u16>,
    /// Data-line timeout exponent (bits[3:0]: timeout = base_clk × 2^(13+N))
    timeout_control: Rw<u8>,
    /// Software Reset (write to assert; controller auto-clears when reset completes)
    software_reset: Rw<u8>,
    /// Normal Interrupt Status (read latched events, write 1s to acknowledge)
    normal_int_status: W1c<u16>,
    /// Error Interrupt Status (read latched error class, write 1s to acknowledge)
    error_int_status: W1c<u16>,
    /// Gate for which NORMAL_INT_STATUS bits the controller latches
    normal_int_status_enable: Rw<u16>,
    /// Gate for which ERROR_INT_STATUS bits the controller latches
    error_int_status_enable: Rw<u16>,
    /// Gate for which NORMAL_INT_STATUS bits raise the IRQ line
    normal_int_signal_enable: Rw<u16>,
    /// Gate for which ERROR_INT_STATUS bits raise the IRQ line
    error_int_signal_enable: Rw<u16>,
    _pad0: [u8; 0x4],
    /// Controller Capabilities (low 32 — hardware-burned)
    capabilities: Ro<u32>,
    /// Controller Capabilities (high 32 — SDR104/SDR50/DDR50/driver-strength/etc.)
    capabilities_hi: Ro<u32>,
    _pad1: [u8; 0x10],
    /// ADMA2 descriptor table physical address (32-bit ADMA2 mode)
    adma_address: Rw<u32>,
    _pad2: [u8; 0xa2],
    /// Host Controller Version (high half of 0x0fc — vendor + spec version)
    host_version: Ro<u16>,
}

// ---------- TransferMode ----------

/// `transfer_mode` half of `transfer_mode_command`'s combined_trigger setter (bits 15:0).
/// Distinct type so the setter's two same-width arguments cannot be
/// positionally swapped: passing `TransferMode(...)` where `<other half>(...)` is
/// expected fails to compile.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct TransferMode(pub u16);

// ---------- Command ----------

/// `command` half of `transfer_mode_command`'s combined_trigger setter (bits 31:16).
/// Distinct type so the setter's two same-width arguments cannot be
/// positionally swapped: passing `Command(...)` where `<other half>(...)` is
/// expected fails to compile.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Command(pub u16);

// ---------- PresentState ----------

/// PresentState register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PresentState(pub u32);

impl PresentState {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// Command transfer in progress — clear before issuing any CMD
    pub const CMD_INHIBIT: Self = Self(1 << 0);
    /// Data transfer in progress / card busy on DAT0 — clear before DAT-using CMDs
    pub const DAT_INHIBIT: Self = Self(1 << 1);
    /// DAT lines actively driving (transfer underway)
    pub const DAT_ACTIVE: Self = Self(1 << 2);
    /// Write transfer underway
    pub const WRITE_TRANSFER_ACTIVE: Self = Self(1 << 8);
    /// Read transfer underway
    pub const READ_TRANSFER_ACTIVE: Self = Self(1 << 9);
    /// Card detected in slot
    pub const CARD_INSERTED: Self = Self(1 << 16);
    /// Card-detect state is stable (debounce settled)
    pub const CARD_STATE_STABLE: Self = Self(1 << 17);
}

impl core::ops::BitOr for PresentState {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for PresentState {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for PresentState {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- HostControl ----------

/// HostControl register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct HostControl(pub u8);

impl HostControl {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u8 { self.0 }
    /// Mask for the `led_on` field (bits 0:0).
    pub const LED_ON_MASK: u8 = 0x1;
    /// Right-shift to access the `led_on` field.
    pub const LED_ON_SHIFT: u32 = 0;
    /// Read `led_on` as bool.
    pub const fn led_on(self) -> bool { (self.0 & Self::LED_ON_MASK) != 0 }
    /// Return a new value with `led_on` set to `v`.
    pub const fn with_led_on(self, v: bool) -> Self {
        if v { Self(self.0 | Self::LED_ON_MASK) } else { Self(self.0 & !Self::LED_ON_MASK) }
    }
    /// Mask for the `dat_4bit` field (bits 1:1).
    pub const DAT_4BIT_MASK: u8 = 0x2;
    /// Right-shift to access the `dat_4bit` field.
    pub const DAT_4BIT_SHIFT: u32 = 1;
    /// Read `dat_4bit` as bool.
    pub const fn dat_4bit(self) -> bool { (self.0 & Self::DAT_4BIT_MASK) != 0 }
    /// Return a new value with `dat_4bit` set to `v`.
    pub const fn with_dat_4bit(self, v: bool) -> Self {
        if v { Self(self.0 | Self::DAT_4BIT_MASK) } else { Self(self.0 & !Self::DAT_4BIT_MASK) }
    }
    /// Mask for the `high_speed` field (bits 2:2).
    pub const HIGH_SPEED_MASK: u8 = 0x4;
    /// Right-shift to access the `high_speed` field.
    pub const HIGH_SPEED_SHIFT: u32 = 2;
    /// Read `high_speed` as bool.
    pub const fn high_speed(self) -> bool { (self.0 & Self::HIGH_SPEED_MASK) != 0 }
    /// Return a new value with `high_speed` set to `v`.
    pub const fn with_high_speed(self, v: bool) -> Self {
        if v { Self(self.0 | Self::HIGH_SPEED_MASK) } else { Self(self.0 & !Self::HIGH_SPEED_MASK) }
    }
    /// Mask for the `dma_sel` field (bits 4:3).
    pub const DMA_SEL_MASK: u8 = 0x18;
    /// Right-shift to access the `dma_sel` field.
    pub const DMA_SEL_SHIFT: u32 = 3;
    /// Decode the `dma_sel` field as `HostControlDmaSel`.
    pub const fn dma_sel(self) -> Result<HostControlDmaSel, ReservedBits> {
        HostControlDmaSel::from_bits((self.0 & Self::DMA_SEL_MASK) >> Self::DMA_SEL_SHIFT)
    }
    /// Return a new value with `dma_sel` set to `v`.
    pub const fn with_dma_sel(self, v: HostControlDmaSel) -> Self {
        Self((self.0 & !Self::DMA_SEL_MASK) | ((v.into_bits() as u8) << Self::DMA_SEL_SHIFT))
    }
}

/// Enum for the `dma_sel` field of `HostControl`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HostControlDmaSel {
    Sdma = 0x0,
    Adma2_32 = 0x2,
    Adma2_64 = 0x3,
}

impl HostControlDmaSel {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u8) -> Result<Self, ReservedBits> {
        match v {
            0x0 => Ok(Self::Sdma),
            0x2 => Ok(Self::Adma2_32),
            0x3 => Ok(Self::Adma2_64),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u8 { self as u8 }
}

// ---------- PowerControl ----------

/// PowerControl register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PowerControl(pub u8);

impl PowerControl {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u8 { self.0 }
    /// Mask for the `bus_power_on` field (bits 0:0).
    pub const BUS_POWER_ON_MASK: u8 = 0x1;
    /// Right-shift to access the `bus_power_on` field.
    pub const BUS_POWER_ON_SHIFT: u32 = 0;
    /// Read `bus_power_on` as bool.
    pub const fn bus_power_on(self) -> bool { (self.0 & Self::BUS_POWER_ON_MASK) != 0 }
    /// Return a new value with `bus_power_on` set to `v`.
    pub const fn with_bus_power_on(self, v: bool) -> Self {
        if v { Self(self.0 | Self::BUS_POWER_ON_MASK) } else { Self(self.0 & !Self::BUS_POWER_ON_MASK) }
    }
    /// Mask for the `bus_voltage` field (bits 3:1).
    pub const BUS_VOLTAGE_MASK: u8 = 0xe;
    /// Right-shift to access the `bus_voltage` field.
    pub const BUS_VOLTAGE_SHIFT: u32 = 1;
    /// Decode the `bus_voltage` field as `PowerControlBusVoltage`.
    pub const fn bus_voltage(self) -> Result<PowerControlBusVoltage, ReservedBits> {
        PowerControlBusVoltage::from_bits((self.0 & Self::BUS_VOLTAGE_MASK) >> Self::BUS_VOLTAGE_SHIFT)
    }
    /// Return a new value with `bus_voltage` set to `v`.
    pub const fn with_bus_voltage(self, v: PowerControlBusVoltage) -> Self {
        Self((self.0 & !Self::BUS_VOLTAGE_MASK) | ((v.into_bits() as u8) << Self::BUS_VOLTAGE_SHIFT))
    }
}

/// Enum for the `bus_voltage` field of `PowerControl`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerControlBusVoltage {
    V18 = 0x5,
    V30 = 0x6,
    V33 = 0x7,
}

impl PowerControlBusVoltage {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u8) -> Result<Self, ReservedBits> {
        match v {
            0x5 => Ok(Self::V18),
            0x6 => Ok(Self::V30),
            0x7 => Ok(Self::V33),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u8 { self as u8 }
}

// ---------- ClockControl ----------

/// ClockControl register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct ClockControl(pub u16);

impl ClockControl {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// Mask for the `int_clk_en` field (bits 0:0).
    pub const INT_CLK_EN_MASK: u16 = 0x1;
    /// Right-shift to access the `int_clk_en` field.
    pub const INT_CLK_EN_SHIFT: u32 = 0;
    /// Read `int_clk_en` as bool.
    pub const fn int_clk_en(self) -> bool { (self.0 & Self::INT_CLK_EN_MASK) != 0 }
    /// Return a new value with `int_clk_en` set to `v`.
    pub const fn with_int_clk_en(self, v: bool) -> Self {
        if v { Self(self.0 | Self::INT_CLK_EN_MASK) } else { Self(self.0 & !Self::INT_CLK_EN_MASK) }
    }
    /// Mask for the `int_clk_stable` field (bits 1:1).
    pub const INT_CLK_STABLE_MASK: u16 = 0x2;
    /// Right-shift to access the `int_clk_stable` field.
    pub const INT_CLK_STABLE_SHIFT: u32 = 1;
    /// Read `int_clk_stable` as bool.
    pub const fn int_clk_stable(self) -> bool { (self.0 & Self::INT_CLK_STABLE_MASK) != 0 }
    /// Mask for the `sd_clk_en` field (bits 2:2).
    pub const SD_CLK_EN_MASK: u16 = 0x4;
    /// Right-shift to access the `sd_clk_en` field.
    pub const SD_CLK_EN_SHIFT: u32 = 2;
    /// Read `sd_clk_en` as bool.
    pub const fn sd_clk_en(self) -> bool { (self.0 & Self::SD_CLK_EN_MASK) != 0 }
    /// Return a new value with `sd_clk_en` set to `v`.
    pub const fn with_sd_clk_en(self, v: bool) -> Self {
        if v { Self(self.0 | Self::SD_CLK_EN_MASK) } else { Self(self.0 & !Self::SD_CLK_EN_MASK) }
    }
    /// Mask for the `freq_sel_upper` field (bits 7:6).
    pub const FREQ_SEL_UPPER_MASK: u16 = 0xc0;
    /// Right-shift to access the `freq_sel_upper` field.
    pub const FREQ_SEL_UPPER_SHIFT: u32 = 6;
    /// Read the `freq_sel_upper` field as a scalar.
    pub const fn freq_sel_upper(self) -> u16 { (self.0 & Self::FREQ_SEL_UPPER_MASK) >> Self::FREQ_SEL_UPPER_SHIFT }
    /// Return a new value with `freq_sel_upper` set to `v` (truncated to field width).
    pub const fn with_freq_sel_upper(self, v: u16) -> Self {
        Self((self.0 & !Self::FREQ_SEL_UPPER_MASK) | ((v << Self::FREQ_SEL_UPPER_SHIFT) & Self::FREQ_SEL_UPPER_MASK))
    }
    /// Mask for the `freq_sel` field (bits 15:8).
    pub const FREQ_SEL_MASK: u16 = 0xff00;
    /// Right-shift to access the `freq_sel` field.
    pub const FREQ_SEL_SHIFT: u32 = 8;
    /// Read the `freq_sel` field as a scalar.
    pub const fn freq_sel(self) -> u16 { (self.0 & Self::FREQ_SEL_MASK) >> Self::FREQ_SEL_SHIFT }
    /// Return a new value with `freq_sel` set to `v` (truncated to field width).
    pub const fn with_freq_sel(self, v: u16) -> Self {
        Self((self.0 & !Self::FREQ_SEL_MASK) | ((v << Self::FREQ_SEL_SHIFT) & Self::FREQ_SEL_MASK))
    }
}

// ---------- TimeoutControl ----------

/// TimeoutControl register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct TimeoutControl(pub u8);

impl TimeoutControl {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u8 { self.0 }
    /// Mask for the `dat_timeout_counter` field (bits 3:0).
    pub const DAT_TIMEOUT_COUNTER_MASK: u8 = 0xf;
    /// Right-shift to access the `dat_timeout_counter` field.
    pub const DAT_TIMEOUT_COUNTER_SHIFT: u32 = 0;
    /// Read the `dat_timeout_counter` field as a scalar.
    pub const fn dat_timeout_counter(self) -> u8 { (self.0 & Self::DAT_TIMEOUT_COUNTER_MASK) >> Self::DAT_TIMEOUT_COUNTER_SHIFT }
    /// Return a new value with `dat_timeout_counter` set to `v` (truncated to field width).
    pub const fn with_dat_timeout_counter(self, v: u8) -> Self {
        Self((self.0 & !Self::DAT_TIMEOUT_COUNTER_MASK) | ((v << Self::DAT_TIMEOUT_COUNTER_SHIFT) & Self::DAT_TIMEOUT_COUNTER_MASK))
    }
}

// ---------- SoftwareReset ----------

/// SoftwareReset register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct SoftwareReset(pub u8);

impl SoftwareReset {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u8 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// Reset entire controller
    pub const SW_RST_ALL: Self = Self(1 << 0);
    /// Reset command circuit only
    pub const SW_RST_CMD: Self = Self(1 << 1);
    /// Reset data circuit only
    pub const SW_RST_DAT: Self = Self(1 << 2);
}

impl core::ops::BitOr for SoftwareReset {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for SoftwareReset {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for SoftwareReset {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- NormalIntStatus ----------

/// NormalIntStatus register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct NormalIntStatus(pub u16);

impl NormalIntStatus {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// CMD/response phase finished
    pub const CMD_COMPLETE: Self = Self(1 << 0);
    /// Data transfer finished
    pub const DATA_COMPLETE: Self = Self(1 << 1);
    /// Block gap event
    pub const BLOCK_GAP: Self = Self(1 << 2);
    /// DMA boundary or completion
    pub const DMA_COMPLETE: Self = Self(1 << 3);
    /// PIO write buffer ready
    pub const BUF_WR_READY: Self = Self(1 << 4);
    /// PIO read buffer ready
    pub const BUF_RD_READY: Self = Self(1 << 5);
    /// Card inserted
    pub const CARD_INSERT: Self = Self(1 << 6);
    /// Card removed
    pub const CARD_REMOVE: Self = Self(1 << 7);
    /// Card SDIO interrupt
    pub const CARD_INT: Self = Self(1 << 8);
    /// Error summary — read ERROR_INT_STATUS for details
    pub const ERROR: Self = Self(1 << 15);
}

impl core::ops::BitOr for NormalIntStatus {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for NormalIntStatus {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for NormalIntStatus {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- ErrorIntStatus ----------

/// ErrorIntStatus register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct ErrorIntStatus(pub u16);

impl ErrorIntStatus {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// Command response timeout
    pub const CMD_TIMEOUT: Self = Self(1 << 0);
    /// Command CRC mismatch
    pub const CMD_CRC: Self = Self(1 << 1);
    /// Command end-bit wrong
    pub const CMD_END_BIT: Self = Self(1 << 2);
    /// Response index doesn't match issued command
    pub const CMD_INDEX: Self = Self(1 << 3);
    /// Data transfer timeout
    pub const DATA_TIMEOUT: Self = Self(1 << 4);
    /// Data CRC mismatch
    pub const DATA_CRC: Self = Self(1 << 5);
    /// Data end-bit wrong
    pub const DATA_END_BIT: Self = Self(1 << 6);
    /// Bus current limit exceeded
    pub const CURRENT_LIMIT: Self = Self(1 << 7);
    /// Auto-CMD12/CMD23 error
    pub const AUTO_CMD: Self = Self(1 << 8);
    /// ADMA2 descriptor or address error
    pub const ADMA: Self = Self(1 << 9);
}

impl core::ops::BitOr for ErrorIntStatus {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for ErrorIntStatus {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for ErrorIntStatus {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- NormalIntStatusEnable ----------

/// NormalIntStatusEnable register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct NormalIntStatusEnable(pub u16);

impl NormalIntStatusEnable {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const CMD_COMPLETE: Self = Self(1 << 0);
    pub const DATA_COMPLETE: Self = Self(1 << 1);
    pub const BLOCK_GAP: Self = Self(1 << 2);
    pub const DMA_COMPLETE: Self = Self(1 << 3);
    pub const BUF_WR_READY: Self = Self(1 << 4);
    pub const BUF_RD_READY: Self = Self(1 << 5);
    pub const CARD_INSERT: Self = Self(1 << 6);
    pub const CARD_REMOVE: Self = Self(1 << 7);
    pub const CARD_INT: Self = Self(1 << 8);
    pub const ERROR: Self = Self(1 << 15);
}

impl core::ops::BitOr for NormalIntStatusEnable {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for NormalIntStatusEnable {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for NormalIntStatusEnable {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- ErrorIntStatusEnable ----------

/// ErrorIntStatusEnable register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct ErrorIntStatusEnable(pub u16);

impl ErrorIntStatusEnable {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const CMD_TIMEOUT: Self = Self(1 << 0);
    pub const CMD_CRC: Self = Self(1 << 1);
    pub const CMD_END_BIT: Self = Self(1 << 2);
    pub const CMD_INDEX: Self = Self(1 << 3);
    pub const DATA_TIMEOUT: Self = Self(1 << 4);
    pub const DATA_CRC: Self = Self(1 << 5);
    pub const DATA_END_BIT: Self = Self(1 << 6);
    pub const CURRENT_LIMIT: Self = Self(1 << 7);
    pub const AUTO_CMD: Self = Self(1 << 8);
    pub const ADMA: Self = Self(1 << 9);
}

impl core::ops::BitOr for ErrorIntStatusEnable {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for ErrorIntStatusEnable {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for ErrorIntStatusEnable {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- NormalIntSignalEnable ----------

/// NormalIntSignalEnable register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct NormalIntSignalEnable(pub u16);

impl NormalIntSignalEnable {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const CMD_COMPLETE: Self = Self(1 << 0);
    pub const DATA_COMPLETE: Self = Self(1 << 1);
    pub const BLOCK_GAP: Self = Self(1 << 2);
    pub const DMA_COMPLETE: Self = Self(1 << 3);
    pub const BUF_WR_READY: Self = Self(1 << 4);
    pub const BUF_RD_READY: Self = Self(1 << 5);
    pub const CARD_INSERT: Self = Self(1 << 6);
    pub const CARD_REMOVE: Self = Self(1 << 7);
    pub const CARD_INT: Self = Self(1 << 8);
    pub const ERROR: Self = Self(1 << 15);
}

impl core::ops::BitOr for NormalIntSignalEnable {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for NormalIntSignalEnable {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for NormalIntSignalEnable {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- ErrorIntSignalEnable ----------

/// ErrorIntSignalEnable register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct ErrorIntSignalEnable(pub u16);

impl ErrorIntSignalEnable {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u16 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const CMD_TIMEOUT: Self = Self(1 << 0);
    pub const CMD_CRC: Self = Self(1 << 1);
    pub const CMD_END_BIT: Self = Self(1 << 2);
    pub const CMD_INDEX: Self = Self(1 << 3);
    pub const DATA_TIMEOUT: Self = Self(1 << 4);
    pub const DATA_CRC: Self = Self(1 << 5);
    pub const DATA_END_BIT: Self = Self(1 << 6);
    pub const CURRENT_LIMIT: Self = Self(1 << 7);
    pub const AUTO_CMD: Self = Self(1 << 8);
    pub const ADMA: Self = Self(1 << 9);
}

impl core::ops::BitOr for ErrorIntSignalEnable {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for ErrorIntSignalEnable {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for ErrorIntSignalEnable {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

// ---------- Capabilities ----------

/// Capabilities register — typed snapshot with field accessors.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Capabilities(pub u32);

impl Capabilities {
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// Mask for the `timeout_clock_freq` field (bits 5:0).
    pub const TIMEOUT_CLOCK_FREQ_MASK: u32 = 0x3f;
    /// Right-shift to access the `timeout_clock_freq` field.
    pub const TIMEOUT_CLOCK_FREQ_SHIFT: u32 = 0;
    /// Read the `timeout_clock_freq` field as a scalar.
    pub const fn timeout_clock_freq(self) -> u32 { (self.0 & Self::TIMEOUT_CLOCK_FREQ_MASK) >> Self::TIMEOUT_CLOCK_FREQ_SHIFT }
    /// Return a new value with `timeout_clock_freq` set to `v` (truncated to field width).
    pub const fn with_timeout_clock_freq(self, v: u32) -> Self {
        Self((self.0 & !Self::TIMEOUT_CLOCK_FREQ_MASK) | ((v << Self::TIMEOUT_CLOCK_FREQ_SHIFT) & Self::TIMEOUT_CLOCK_FREQ_MASK))
    }
    /// Mask for the `base_clock_mhz` field (bits 15:8).
    pub const BASE_CLOCK_MHZ_MASK: u32 = 0xff00;
    /// Right-shift to access the `base_clock_mhz` field.
    pub const BASE_CLOCK_MHZ_SHIFT: u32 = 8;
    /// Read the `base_clock_mhz` field as a scalar.
    pub const fn base_clock_mhz(self) -> u32 { (self.0 & Self::BASE_CLOCK_MHZ_MASK) >> Self::BASE_CLOCK_MHZ_SHIFT }
    /// Return a new value with `base_clock_mhz` set to `v` (truncated to field width).
    pub const fn with_base_clock_mhz(self, v: u32) -> Self {
        Self((self.0 & !Self::BASE_CLOCK_MHZ_MASK) | ((v << Self::BASE_CLOCK_MHZ_SHIFT) & Self::BASE_CLOCK_MHZ_MASK))
    }
    /// Mask for the `max_block_len` field (bits 17:16).
    pub const MAX_BLOCK_LEN_MASK: u32 = 0x30000;
    /// Right-shift to access the `max_block_len` field.
    pub const MAX_BLOCK_LEN_SHIFT: u32 = 16;
    /// Decode the `max_block_len` field as `CapabilitiesMaxBlockLen`.
    pub const fn max_block_len(self) -> Result<CapabilitiesMaxBlockLen, ReservedBits> {
        CapabilitiesMaxBlockLen::from_bits((self.0 & Self::MAX_BLOCK_LEN_MASK) >> Self::MAX_BLOCK_LEN_SHIFT)
    }
    /// Return a new value with `max_block_len` set to `v`.
    pub const fn with_max_block_len(self, v: CapabilitiesMaxBlockLen) -> Self {
        Self((self.0 & !Self::MAX_BLOCK_LEN_MASK) | ((v.into_bits() as u32) << Self::MAX_BLOCK_LEN_SHIFT))
    }
    /// Mask for the `adma2_supported` field (bits 19:19).
    pub const ADMA2_SUPPORTED_MASK: u32 = 0x80000;
    /// Right-shift to access the `adma2_supported` field.
    pub const ADMA2_SUPPORTED_SHIFT: u32 = 19;
    /// Read `adma2_supported` as bool.
    pub const fn adma2_supported(self) -> bool { (self.0 & Self::ADMA2_SUPPORTED_MASK) != 0 }
    /// Return a new value with `adma2_supported` set to `v`.
    pub const fn with_adma2_supported(self, v: bool) -> Self {
        if v { Self(self.0 | Self::ADMA2_SUPPORTED_MASK) } else { Self(self.0 & !Self::ADMA2_SUPPORTED_MASK) }
    }
    /// Mask for the `high_speed_supported` field (bits 21:21).
    pub const HIGH_SPEED_SUPPORTED_MASK: u32 = 0x200000;
    /// Right-shift to access the `high_speed_supported` field.
    pub const HIGH_SPEED_SUPPORTED_SHIFT: u32 = 21;
    /// Read `high_speed_supported` as bool.
    pub const fn high_speed_supported(self) -> bool { (self.0 & Self::HIGH_SPEED_SUPPORTED_MASK) != 0 }
    /// Return a new value with `high_speed_supported` set to `v`.
    pub const fn with_high_speed_supported(self, v: bool) -> Self {
        if v { Self(self.0 | Self::HIGH_SPEED_SUPPORTED_MASK) } else { Self(self.0 & !Self::HIGH_SPEED_SUPPORTED_MASK) }
    }
    /// Mask for the `voltage_330` field (bits 24:24).
    pub const VOLTAGE_330_MASK: u32 = 0x1000000;
    /// Right-shift to access the `voltage_330` field.
    pub const VOLTAGE_330_SHIFT: u32 = 24;
    /// Read `voltage_330` as bool.
    pub const fn voltage_330(self) -> bool { (self.0 & Self::VOLTAGE_330_MASK) != 0 }
    /// Return a new value with `voltage_330` set to `v`.
    pub const fn with_voltage_330(self, v: bool) -> Self {
        if v { Self(self.0 | Self::VOLTAGE_330_MASK) } else { Self(self.0 & !Self::VOLTAGE_330_MASK) }
    }
    /// Mask for the `voltage_300` field (bits 25:25).
    pub const VOLTAGE_300_MASK: u32 = 0x2000000;
    /// Right-shift to access the `voltage_300` field.
    pub const VOLTAGE_300_SHIFT: u32 = 25;
    /// Read `voltage_300` as bool.
    pub const fn voltage_300(self) -> bool { (self.0 & Self::VOLTAGE_300_MASK) != 0 }
    /// Return a new value with `voltage_300` set to `v`.
    pub const fn with_voltage_300(self, v: bool) -> Self {
        if v { Self(self.0 | Self::VOLTAGE_300_MASK) } else { Self(self.0 & !Self::VOLTAGE_300_MASK) }
    }
    /// Mask for the `voltage_180` field (bits 26:26).
    pub const VOLTAGE_180_MASK: u32 = 0x4000000;
    /// Right-shift to access the `voltage_180` field.
    pub const VOLTAGE_180_SHIFT: u32 = 26;
    /// Read `voltage_180` as bool.
    pub const fn voltage_180(self) -> bool { (self.0 & Self::VOLTAGE_180_MASK) != 0 }
    /// Return a new value with `voltage_180` set to `v`.
    pub const fn with_voltage_180(self, v: bool) -> Self {
        if v { Self(self.0 | Self::VOLTAGE_180_MASK) } else { Self(self.0 & !Self::VOLTAGE_180_MASK) }
    }
}

/// Enum for the `max_block_len` field of `Capabilities`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CapabilitiesMaxBlockLen {
    B512 = 0x0,
    B1024 = 0x1,
    B2048 = 0x2,
}

impl CapabilitiesMaxBlockLen {
    /// Decode from raw bits. Returns `Err(ReservedBits)` if the
    /// pattern is not a defined variant.
    pub const fn from_bits(v: u32) -> Result<Self, ReservedBits> {
        match v {
            0x0 => Ok(Self::B512),
            0x1 => Ok(Self::B1024),
            0x2 => Ok(Self::B2048),
            _ => Err(ReservedBits(v as u64)),
        }
    }
    /// Encode to raw bits.
    pub const fn into_bits(self) -> u32 { self as u32 }
}

/// Returned when an enum decode sees a bit pattern that does not
/// correspond to any declared variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReservedBits(pub u64);

// ---------- Sdhci operation-layer capability tokens ----------

// Capability gate for the dangerous Sdhci accessors. The token has
// no public struct-literal constructor; its only mint paths are the
// two `pub fn`s below, both `pub` so `lockjaw-userlib` can reach
// them. Drivers cannot import `lockjaw_regs` (enforced by the
// `check-driver-unsafe` xtask), so the mint paths are unreachable
// from driver source. Generated by `cargo xtask gen-regs` from the
// `requires_token = "..."` annotations in the regspec.

/// Capability token for the `sdhci_op` gated-accessor regime.
///
/// The lifetime `'a` ties the token to a specific `&'a Sdhci` borrow,
/// so a token cannot outlive the controller reference it was minted
/// against. The private `_no_ctor` field blocks struct-literal
/// construction from outside this module.
pub struct SdhciOpToken<'a> {
    _life: core::marker::PhantomData<&'a Sdhci>,
    _no_ctor: (),
}

/// Mint a `SdhciOpToken` for use inside `lockjaw-userlib`'s operation-layer
/// envelope. **Do not call this from driver source.** The xtask
/// `check-driver-unsafe` denies any `lockjaw_regs` import in driver
/// crates, so this name is unreachable from `user/*-driver/`.
#[inline(always)]
pub fn __sdhci_internal_mint(_dev: &Sdhci) -> SdhciOpToken<'_> {
    SdhciOpToken { _life: core::marker::PhantomData, _no_ctor: () }
}

/// Temporary unguarded mint — exists ONLY for the O2-O5 migration
/// window. emmc2-driver calls this while its gated-setter sites are
/// being rewritten to flow through `lockjaw-userlib::sdhci::SdhciCommandInit`
/// and the init-time helpers. Deleted in O5 once data-phase migrates.
#[deprecated(note = "O2-O5 migration escape; removed in O5")]
#[inline(always)]
pub fn __temp_unguarded_mint(_dev: &Sdhci) -> SdhciOpToken<'_> {
    SdhciOpToken { _life: core::marker::PhantomData, _no_ctor: () }
}

// ---------- Sdhci accessors ----------

impl Sdhci {
    /// Volatile read of `sysaddr` as `u32`.
    #[inline(always)]
    pub fn read_sysaddr(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.sysaddr.read() }
    /// Volatile write of `sysaddr`.
    #[inline(always)]
    pub fn write_sysaddr(&self, v: u32, _tk: &SdhciOpToken<'_>) { self.sysaddr.write(v); }
    /// Read-modify-write `sysaddr`.
    #[inline(always)]
    pub fn modify_sysaddr<F: FnOnce(u32) -> u32>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.sysaddr.modify(f);
    }

    /// `argument2` — alias of `sysaddr`; both names route through the same cell.
    #[inline(always)]
    pub fn read_argument2(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.read_sysaddr(_tk) }
    #[inline(always)]
    pub fn write_argument2(&self, v: u32, _tk: &SdhciOpToken<'_>) { self.write_sysaddr(v, _tk); }
    #[inline(always)]
    pub fn modify_argument2<F: FnOnce(u32) -> u32>(&self, f: F, _tk: &SdhciOpToken<'_>) { self.modify_sysaddr(f, _tk); }
    /// Volatile read of `block_size` as `u16`.
    #[inline(always)]
    pub fn read_block_size(&self, _tk: &SdhciOpToken<'_>) -> u16 { self.block_size.read() }
    /// Volatile write of `block_size`.
    #[inline(always)]
    pub fn write_block_size(&self, v: u16, _tk: &SdhciOpToken<'_>) { self.block_size.write(v); }
    /// Read-modify-write `block_size`.
    #[inline(always)]
    pub fn modify_block_size<F: FnOnce(u16) -> u16>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.block_size.modify(f);
    }
    /// Volatile read of `block_count` as `u16`.
    #[inline(always)]
    pub fn read_block_count(&self, _tk: &SdhciOpToken<'_>) -> u16 { self.block_count.read() }
    /// Volatile write of `block_count`.
    #[inline(always)]
    pub fn write_block_count(&self, v: u16, _tk: &SdhciOpToken<'_>) { self.block_count.write(v); }
    /// Read-modify-write `block_count`.
    #[inline(always)]
    pub fn modify_block_count<F: FnOnce(u16) -> u16>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.block_count.modify(f);
    }
    /// Volatile read of `argument` as `u32`.
    #[inline(always)]
    pub fn read_argument(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.argument.read() }
    /// Volatile write of `argument`.
    #[inline(always)]
    pub fn write_argument(&self, v: u32, _tk: &SdhciOpToken<'_>) { self.argument.write(v); }
    /// Read-modify-write `argument`.
    #[inline(always)]
    pub fn modify_argument<F: FnOnce(u32) -> u32>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.argument.modify(f);
    }
    /// Fire the combined trigger — composes the parts into a SINGLE u32 store.
    /// The single-store property is load-bearing: BCM2711 SDHCI silently drops
    /// the command if the write is split into two halves. The codegen emits one
    /// `Wo<u32>::write(...)` call so the driver cannot accidentally produce two.
    /// Typed per-part newtypes (P9.0b) make positional swap a compile error.
    #[inline(always)]
    pub fn set_transfer_mode_command(&self, transfer_mode: TransferMode, command: Command, _tk: &SdhciOpToken<'_>) {
        self.transfer_mode_command.write(0u32 | (((transfer_mode.0) as u32) << 0) | (((command.0) as u32) << 16));
    }
    /// Volatile read of `response_0` as `u32`.
    #[inline(always)]
    pub fn read_response_0(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.response_0.read() }
    /// Volatile read of `response_1` as `u32`.
    #[inline(always)]
    pub fn read_response_1(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.response_1.read() }
    /// Volatile read of `response_2` as `u32`.
    #[inline(always)]
    pub fn read_response_2(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.response_2.read() }
    /// Volatile read of `response_3` as `u32`.
    #[inline(always)]
    pub fn read_response_3(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.response_3.read() }
    /// Volatile read of `buffer_data_port` as `u32`.
    #[inline(always)]
    pub fn read_buffer_data_port(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.buffer_data_port.read() }
    /// Volatile write of `buffer_data_port`.
    #[inline(always)]
    pub fn write_buffer_data_port(&self, v: u32, _tk: &SdhciOpToken<'_>) { self.buffer_data_port.write(v); }
    /// Read-modify-write `buffer_data_port`.
    #[inline(always)]
    pub fn modify_buffer_data_port<F: FnOnce(u32) -> u32>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.buffer_data_port.modify(f);
    }
    /// Read a typed snapshot of `present_state`.
    #[inline(always)]
    pub fn present_state(&self) -> PresentState { PresentState(self.present_state.read()) }
    /// Read a typed snapshot of `host_control`.
    #[inline(always)]
    pub fn host_control(&self, _tk: &SdhciOpToken<'_>) -> HostControl { HostControl(self.host_control.read()) }
    /// Write the value back to `host_control`.
    #[inline(always)]
    pub fn set_host_control(&self, v: HostControl, _tk: &SdhciOpToken<'_>) { self.host_control.write(v.0); }
    /// Read-modify-write `host_control` via a typed closure.
    #[inline(always)]
    pub fn modify_host_control<F: FnOnce(HostControl) -> HostControl>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.host_control.modify(|v| f(HostControl(v)).0);
    }
    /// Read a typed snapshot of `power_control`.
    #[inline(always)]
    pub fn power_control(&self, _tk: &SdhciOpToken<'_>) -> PowerControl { PowerControl(self.power_control.read()) }
    /// Write the value back to `power_control`.
    #[inline(always)]
    pub fn set_power_control(&self, v: PowerControl, _tk: &SdhciOpToken<'_>) { self.power_control.write(v.0); }
    /// Read-modify-write `power_control` via a typed closure.
    #[inline(always)]
    pub fn modify_power_control<F: FnOnce(PowerControl) -> PowerControl>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.power_control.modify(|v| f(PowerControl(v)).0);
    }
    /// Volatile read of `block_gap_control` as `u8`.
    #[inline(always)]
    pub fn read_block_gap_control(&self) -> u8 { self.block_gap_control.read() }
    /// Volatile write of `block_gap_control`.
    #[inline(always)]
    pub fn write_block_gap_control(&self, v: u8) { self.block_gap_control.write(v); }
    /// Read-modify-write `block_gap_control`.
    #[inline(always)]
    pub fn modify_block_gap_control<F: FnOnce(u8) -> u8>(&self, f: F) {
        self.block_gap_control.modify(f);
    }
    /// Volatile read of `wakeup_control` as `u8`.
    #[inline(always)]
    pub fn read_wakeup_control(&self) -> u8 { self.wakeup_control.read() }
    /// Volatile write of `wakeup_control`.
    #[inline(always)]
    pub fn write_wakeup_control(&self, v: u8) { self.wakeup_control.write(v); }
    /// Read-modify-write `wakeup_control`.
    #[inline(always)]
    pub fn modify_wakeup_control<F: FnOnce(u8) -> u8>(&self, f: F) {
        self.wakeup_control.modify(f);
    }
    /// Read a typed snapshot of `clock_control`.
    #[inline(always)]
    pub fn clock_control(&self, _tk: &SdhciOpToken<'_>) -> ClockControl { ClockControl(self.clock_control.read()) }
    /// Write the value back to `clock_control`.
    #[inline(always)]
    pub fn set_clock_control(&self, v: ClockControl, _tk: &SdhciOpToken<'_>) { self.clock_control.write(v.0); }
    /// Read-modify-write `clock_control` via a typed closure.
    #[inline(always)]
    pub fn modify_clock_control<F: FnOnce(ClockControl) -> ClockControl>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.clock_control.modify(|v| f(ClockControl(v)).0);
    }
    /// Read a typed snapshot of `timeout_control`.
    #[inline(always)]
    pub fn timeout_control(&self, _tk: &SdhciOpToken<'_>) -> TimeoutControl { TimeoutControl(self.timeout_control.read()) }
    /// Write the value back to `timeout_control`.
    #[inline(always)]
    pub fn set_timeout_control(&self, v: TimeoutControl, _tk: &SdhciOpToken<'_>) { self.timeout_control.write(v.0); }
    /// Read-modify-write `timeout_control` via a typed closure.
    #[inline(always)]
    pub fn modify_timeout_control<F: FnOnce(TimeoutControl) -> TimeoutControl>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.timeout_control.modify(|v| f(TimeoutControl(v)).0);
    }
    /// Read a typed snapshot of `software_reset`.
    #[inline(always)]
    pub fn software_reset(&self, _tk: &SdhciOpToken<'_>) -> SoftwareReset { SoftwareReset(self.software_reset.read()) }
    /// Write the value back to `software_reset`.
    #[inline(always)]
    pub fn set_software_reset(&self, v: SoftwareReset, _tk: &SdhciOpToken<'_>) { self.software_reset.write(v.0); }
    /// Read-modify-write `software_reset` via a typed closure.
    #[inline(always)]
    pub fn modify_software_reset<F: FnOnce(SoftwareReset) -> SoftwareReset>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.software_reset.modify(|v| f(SoftwareReset(v)).0);
    }
    /// Read a typed snapshot of `normal_int_status` (which bits are currently latched).
    #[inline(always)]
    pub fn normal_int_status(&self, _tk: &SdhciOpToken<'_>) -> NormalIntStatus { NormalIntStatus(self.normal_int_status.read()) }
    /// Clear bits in `normal_int_status` (write-1-to-clear); pass the typed mask of bits to ack.
    #[inline(always)]
    pub fn clear_normal_int_status(&self, mask: NormalIntStatus, _tk: &SdhciOpToken<'_>) { self.normal_int_status.clear(mask.0); }
    /// Read a typed snapshot of `error_int_status` (which bits are currently latched).
    #[inline(always)]
    pub fn error_int_status(&self, _tk: &SdhciOpToken<'_>) -> ErrorIntStatus { ErrorIntStatus(self.error_int_status.read()) }
    /// Clear bits in `error_int_status` (write-1-to-clear); pass the typed mask of bits to ack.
    #[inline(always)]
    pub fn clear_error_int_status(&self, mask: ErrorIntStatus, _tk: &SdhciOpToken<'_>) { self.error_int_status.clear(mask.0); }
    /// Read a typed snapshot of `normal_int_status_enable`.
    #[inline(always)]
    pub fn normal_int_status_enable(&self, _tk: &SdhciOpToken<'_>) -> NormalIntStatusEnable { NormalIntStatusEnable(self.normal_int_status_enable.read()) }
    /// Write the value back to `normal_int_status_enable`.
    #[inline(always)]
    pub fn set_normal_int_status_enable(&self, v: NormalIntStatusEnable, _tk: &SdhciOpToken<'_>) { self.normal_int_status_enable.write(v.0); }
    /// Read-modify-write `normal_int_status_enable` via a typed closure.
    #[inline(always)]
    pub fn modify_normal_int_status_enable<F: FnOnce(NormalIntStatusEnable) -> NormalIntStatusEnable>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.normal_int_status_enable.modify(|v| f(NormalIntStatusEnable(v)).0);
    }
    /// Read a typed snapshot of `error_int_status_enable`.
    #[inline(always)]
    pub fn error_int_status_enable(&self, _tk: &SdhciOpToken<'_>) -> ErrorIntStatusEnable { ErrorIntStatusEnable(self.error_int_status_enable.read()) }
    /// Write the value back to `error_int_status_enable`.
    #[inline(always)]
    pub fn set_error_int_status_enable(&self, v: ErrorIntStatusEnable, _tk: &SdhciOpToken<'_>) { self.error_int_status_enable.write(v.0); }
    /// Read-modify-write `error_int_status_enable` via a typed closure.
    #[inline(always)]
    pub fn modify_error_int_status_enable<F: FnOnce(ErrorIntStatusEnable) -> ErrorIntStatusEnable>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.error_int_status_enable.modify(|v| f(ErrorIntStatusEnable(v)).0);
    }
    /// Read a typed snapshot of `normal_int_signal_enable`.
    #[inline(always)]
    pub fn normal_int_signal_enable(&self, _tk: &SdhciOpToken<'_>) -> NormalIntSignalEnable { NormalIntSignalEnable(self.normal_int_signal_enable.read()) }
    /// Write the value back to `normal_int_signal_enable`.
    #[inline(always)]
    pub fn set_normal_int_signal_enable(&self, v: NormalIntSignalEnable, _tk: &SdhciOpToken<'_>) { self.normal_int_signal_enable.write(v.0); }
    /// Read-modify-write `normal_int_signal_enable` via a typed closure.
    #[inline(always)]
    pub fn modify_normal_int_signal_enable<F: FnOnce(NormalIntSignalEnable) -> NormalIntSignalEnable>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.normal_int_signal_enable.modify(|v| f(NormalIntSignalEnable(v)).0);
    }
    /// Read a typed snapshot of `error_int_signal_enable`.
    #[inline(always)]
    pub fn error_int_signal_enable(&self, _tk: &SdhciOpToken<'_>) -> ErrorIntSignalEnable { ErrorIntSignalEnable(self.error_int_signal_enable.read()) }
    /// Write the value back to `error_int_signal_enable`.
    #[inline(always)]
    pub fn set_error_int_signal_enable(&self, v: ErrorIntSignalEnable, _tk: &SdhciOpToken<'_>) { self.error_int_signal_enable.write(v.0); }
    /// Read-modify-write `error_int_signal_enable` via a typed closure.
    #[inline(always)]
    pub fn modify_error_int_signal_enable<F: FnOnce(ErrorIntSignalEnable) -> ErrorIntSignalEnable>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.error_int_signal_enable.modify(|v| f(ErrorIntSignalEnable(v)).0);
    }
    /// Read a typed snapshot of `capabilities`.
    #[inline(always)]
    pub fn capabilities(&self) -> Capabilities { Capabilities(self.capabilities.read()) }
    /// Volatile read of `capabilities_hi` as `u32`.
    #[inline(always)]
    pub fn read_capabilities_hi(&self) -> u32 { self.capabilities_hi.read() }
    /// Volatile read of `adma_address` as `u32`.
    #[inline(always)]
    pub fn read_adma_address(&self, _tk: &SdhciOpToken<'_>) -> u32 { self.adma_address.read() }
    /// Volatile write of `adma_address`.
    #[inline(always)]
    pub fn write_adma_address(&self, v: u32, _tk: &SdhciOpToken<'_>) { self.adma_address.write(v); }
    /// Read-modify-write `adma_address`.
    #[inline(always)]
    pub fn modify_adma_address<F: FnOnce(u32) -> u32>(&self, f: F, _tk: &SdhciOpToken<'_>) {
        self.adma_address.modify(f);
    }
    /// Volatile read of `host_version` as `u16`.
    #[inline(always)]
    pub fn read_host_version(&self) -> u16 { self.host_version.read() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use lockjaw_mmio::mock::MockMmioRegion;

    #[test]
    fn layout_offsets() {
        assert_eq!(offset_of!(Sdhci, sysaddr), 0x0, "sysaddr offset");
        assert_eq!(offset_of!(Sdhci, block_size), 0x4, "block_size offset");
        assert_eq!(offset_of!(Sdhci, block_count), 0x6, "block_count offset");
        assert_eq!(offset_of!(Sdhci, argument), 0x8, "argument offset");
        assert_eq!(offset_of!(Sdhci, transfer_mode_command), 0xc, "transfer_mode_command offset");
        assert_eq!(offset_of!(Sdhci, response_0), 0x10, "response_0 offset");
        assert_eq!(offset_of!(Sdhci, response_1), 0x14, "response_1 offset");
        assert_eq!(offset_of!(Sdhci, response_2), 0x18, "response_2 offset");
        assert_eq!(offset_of!(Sdhci, response_3), 0x1c, "response_3 offset");
        assert_eq!(offset_of!(Sdhci, buffer_data_port), 0x20, "buffer_data_port offset");
        assert_eq!(offset_of!(Sdhci, present_state), 0x24, "present_state offset");
        assert_eq!(offset_of!(Sdhci, host_control), 0x28, "host_control offset");
        assert_eq!(offset_of!(Sdhci, power_control), 0x29, "power_control offset");
        assert_eq!(offset_of!(Sdhci, block_gap_control), 0x2a, "block_gap_control offset");
        assert_eq!(offset_of!(Sdhci, wakeup_control), 0x2b, "wakeup_control offset");
        assert_eq!(offset_of!(Sdhci, clock_control), 0x2c, "clock_control offset");
        assert_eq!(offset_of!(Sdhci, timeout_control), 0x2e, "timeout_control offset");
        assert_eq!(offset_of!(Sdhci, software_reset), 0x2f, "software_reset offset");
        assert_eq!(offset_of!(Sdhci, normal_int_status), 0x30, "normal_int_status offset");
        assert_eq!(offset_of!(Sdhci, error_int_status), 0x32, "error_int_status offset");
        assert_eq!(offset_of!(Sdhci, normal_int_status_enable), 0x34, "normal_int_status_enable offset");
        assert_eq!(offset_of!(Sdhci, error_int_status_enable), 0x36, "error_int_status_enable offset");
        assert_eq!(offset_of!(Sdhci, normal_int_signal_enable), 0x38, "normal_int_signal_enable offset");
        assert_eq!(offset_of!(Sdhci, error_int_signal_enable), 0x3a, "error_int_signal_enable offset");
        assert_eq!(offset_of!(Sdhci, capabilities), 0x40, "capabilities offset");
        assert_eq!(offset_of!(Sdhci, capabilities_hi), 0x44, "capabilities_hi offset");
        assert_eq!(offset_of!(Sdhci, adma_address), 0x58, "adma_address offset");
        assert_eq!(offset_of!(Sdhci, host_version), 0xfe, "host_version offset");
    }

    #[test]
    fn present_state_flag_bits() {
        assert_eq!(PresentState::CMD_INHIBIT.bits(), 1 << 0, "cmd_inhibit");
        assert_eq!(PresentState::DAT_INHIBIT.bits(), 1 << 1, "dat_inhibit");
        assert_eq!(PresentState::DAT_ACTIVE.bits(), 1 << 2, "dat_active");
        assert_eq!(PresentState::WRITE_TRANSFER_ACTIVE.bits(), 1 << 8, "write_transfer_active");
        assert_eq!(PresentState::READ_TRANSFER_ACTIVE.bits(), 1 << 9, "read_transfer_active");
        assert_eq!(PresentState::CARD_INSERTED.bits(), 1 << 16, "card_inserted");
        assert_eq!(PresentState::CARD_STATE_STABLE.bits(), 1 << 17, "card_state_stable");
    }

    #[test]
    fn present_state_flag_compose() {
        let v = PresentState::CMD_INHIBIT | PresentState::DAT_INHIBIT;
        assert!(v.contains(PresentState::CMD_INHIBIT));
        assert!(v.contains(PresentState::DAT_INHIBIT));
        assert!(!PresentState::CMD_INHIBIT.contains(PresentState::DAT_INHIBIT));
    }

    #[test]
    fn software_reset_flag_bits() {
        assert_eq!(SoftwareReset::SW_RST_ALL.bits(), 1 << 0, "sw_rst_all");
        assert_eq!(SoftwareReset::SW_RST_CMD.bits(), 1 << 1, "sw_rst_cmd");
        assert_eq!(SoftwareReset::SW_RST_DAT.bits(), 1 << 2, "sw_rst_dat");
    }

    #[test]
    fn software_reset_flag_compose() {
        let v = SoftwareReset::SW_RST_ALL | SoftwareReset::SW_RST_CMD;
        assert!(v.contains(SoftwareReset::SW_RST_ALL));
        assert!(v.contains(SoftwareReset::SW_RST_CMD));
        assert!(!SoftwareReset::SW_RST_ALL.contains(SoftwareReset::SW_RST_CMD));
    }

    #[test]
    fn normal_int_status_flag_bits() {
        assert_eq!(NormalIntStatus::CMD_COMPLETE.bits(), 1 << 0, "cmd_complete");
        assert_eq!(NormalIntStatus::DATA_COMPLETE.bits(), 1 << 1, "data_complete");
        assert_eq!(NormalIntStatus::BLOCK_GAP.bits(), 1 << 2, "block_gap");
        assert_eq!(NormalIntStatus::DMA_COMPLETE.bits(), 1 << 3, "dma_complete");
        assert_eq!(NormalIntStatus::BUF_WR_READY.bits(), 1 << 4, "buf_wr_ready");
        assert_eq!(NormalIntStatus::BUF_RD_READY.bits(), 1 << 5, "buf_rd_ready");
        assert_eq!(NormalIntStatus::CARD_INSERT.bits(), 1 << 6, "card_insert");
        assert_eq!(NormalIntStatus::CARD_REMOVE.bits(), 1 << 7, "card_remove");
        assert_eq!(NormalIntStatus::CARD_INT.bits(), 1 << 8, "card_int");
        assert_eq!(NormalIntStatus::ERROR.bits(), 1 << 15, "error");
    }

    #[test]
    fn normal_int_status_flag_compose() {
        let v = NormalIntStatus::CMD_COMPLETE | NormalIntStatus::DATA_COMPLETE;
        assert!(v.contains(NormalIntStatus::CMD_COMPLETE));
        assert!(v.contains(NormalIntStatus::DATA_COMPLETE));
        assert!(!NormalIntStatus::CMD_COMPLETE.contains(NormalIntStatus::DATA_COMPLETE));
    }

    #[test]
    fn error_int_status_flag_bits() {
        assert_eq!(ErrorIntStatus::CMD_TIMEOUT.bits(), 1 << 0, "cmd_timeout");
        assert_eq!(ErrorIntStatus::CMD_CRC.bits(), 1 << 1, "cmd_crc");
        assert_eq!(ErrorIntStatus::CMD_END_BIT.bits(), 1 << 2, "cmd_end_bit");
        assert_eq!(ErrorIntStatus::CMD_INDEX.bits(), 1 << 3, "cmd_index");
        assert_eq!(ErrorIntStatus::DATA_TIMEOUT.bits(), 1 << 4, "data_timeout");
        assert_eq!(ErrorIntStatus::DATA_CRC.bits(), 1 << 5, "data_crc");
        assert_eq!(ErrorIntStatus::DATA_END_BIT.bits(), 1 << 6, "data_end_bit");
        assert_eq!(ErrorIntStatus::CURRENT_LIMIT.bits(), 1 << 7, "current_limit");
        assert_eq!(ErrorIntStatus::AUTO_CMD.bits(), 1 << 8, "auto_cmd");
        assert_eq!(ErrorIntStatus::ADMA.bits(), 1 << 9, "adma");
    }

    #[test]
    fn error_int_status_flag_compose() {
        let v = ErrorIntStatus::CMD_TIMEOUT | ErrorIntStatus::CMD_CRC;
        assert!(v.contains(ErrorIntStatus::CMD_TIMEOUT));
        assert!(v.contains(ErrorIntStatus::CMD_CRC));
        assert!(!ErrorIntStatus::CMD_TIMEOUT.contains(ErrorIntStatus::CMD_CRC));
    }

    #[test]
    fn normal_int_status_enable_flag_bits() {
        assert_eq!(NormalIntStatusEnable::CMD_COMPLETE.bits(), 1 << 0, "cmd_complete");
        assert_eq!(NormalIntStatusEnable::DATA_COMPLETE.bits(), 1 << 1, "data_complete");
        assert_eq!(NormalIntStatusEnable::BLOCK_GAP.bits(), 1 << 2, "block_gap");
        assert_eq!(NormalIntStatusEnable::DMA_COMPLETE.bits(), 1 << 3, "dma_complete");
        assert_eq!(NormalIntStatusEnable::BUF_WR_READY.bits(), 1 << 4, "buf_wr_ready");
        assert_eq!(NormalIntStatusEnable::BUF_RD_READY.bits(), 1 << 5, "buf_rd_ready");
        assert_eq!(NormalIntStatusEnable::CARD_INSERT.bits(), 1 << 6, "card_insert");
        assert_eq!(NormalIntStatusEnable::CARD_REMOVE.bits(), 1 << 7, "card_remove");
        assert_eq!(NormalIntStatusEnable::CARD_INT.bits(), 1 << 8, "card_int");
        assert_eq!(NormalIntStatusEnable::ERROR.bits(), 1 << 15, "error");
    }

    #[test]
    fn normal_int_status_enable_flag_compose() {
        let v = NormalIntStatusEnable::CMD_COMPLETE | NormalIntStatusEnable::DATA_COMPLETE;
        assert!(v.contains(NormalIntStatusEnable::CMD_COMPLETE));
        assert!(v.contains(NormalIntStatusEnable::DATA_COMPLETE));
        assert!(!NormalIntStatusEnable::CMD_COMPLETE.contains(NormalIntStatusEnable::DATA_COMPLETE));
    }

    #[test]
    fn error_int_status_enable_flag_bits() {
        assert_eq!(ErrorIntStatusEnable::CMD_TIMEOUT.bits(), 1 << 0, "cmd_timeout");
        assert_eq!(ErrorIntStatusEnable::CMD_CRC.bits(), 1 << 1, "cmd_crc");
        assert_eq!(ErrorIntStatusEnable::CMD_END_BIT.bits(), 1 << 2, "cmd_end_bit");
        assert_eq!(ErrorIntStatusEnable::CMD_INDEX.bits(), 1 << 3, "cmd_index");
        assert_eq!(ErrorIntStatusEnable::DATA_TIMEOUT.bits(), 1 << 4, "data_timeout");
        assert_eq!(ErrorIntStatusEnable::DATA_CRC.bits(), 1 << 5, "data_crc");
        assert_eq!(ErrorIntStatusEnable::DATA_END_BIT.bits(), 1 << 6, "data_end_bit");
        assert_eq!(ErrorIntStatusEnable::CURRENT_LIMIT.bits(), 1 << 7, "current_limit");
        assert_eq!(ErrorIntStatusEnable::AUTO_CMD.bits(), 1 << 8, "auto_cmd");
        assert_eq!(ErrorIntStatusEnable::ADMA.bits(), 1 << 9, "adma");
    }

    #[test]
    fn error_int_status_enable_flag_compose() {
        let v = ErrorIntStatusEnable::CMD_TIMEOUT | ErrorIntStatusEnable::CMD_CRC;
        assert!(v.contains(ErrorIntStatusEnable::CMD_TIMEOUT));
        assert!(v.contains(ErrorIntStatusEnable::CMD_CRC));
        assert!(!ErrorIntStatusEnable::CMD_TIMEOUT.contains(ErrorIntStatusEnable::CMD_CRC));
    }

    #[test]
    fn normal_int_signal_enable_flag_bits() {
        assert_eq!(NormalIntSignalEnable::CMD_COMPLETE.bits(), 1 << 0, "cmd_complete");
        assert_eq!(NormalIntSignalEnable::DATA_COMPLETE.bits(), 1 << 1, "data_complete");
        assert_eq!(NormalIntSignalEnable::BLOCK_GAP.bits(), 1 << 2, "block_gap");
        assert_eq!(NormalIntSignalEnable::DMA_COMPLETE.bits(), 1 << 3, "dma_complete");
        assert_eq!(NormalIntSignalEnable::BUF_WR_READY.bits(), 1 << 4, "buf_wr_ready");
        assert_eq!(NormalIntSignalEnable::BUF_RD_READY.bits(), 1 << 5, "buf_rd_ready");
        assert_eq!(NormalIntSignalEnable::CARD_INSERT.bits(), 1 << 6, "card_insert");
        assert_eq!(NormalIntSignalEnable::CARD_REMOVE.bits(), 1 << 7, "card_remove");
        assert_eq!(NormalIntSignalEnable::CARD_INT.bits(), 1 << 8, "card_int");
        assert_eq!(NormalIntSignalEnable::ERROR.bits(), 1 << 15, "error");
    }

    #[test]
    fn normal_int_signal_enable_flag_compose() {
        let v = NormalIntSignalEnable::CMD_COMPLETE | NormalIntSignalEnable::DATA_COMPLETE;
        assert!(v.contains(NormalIntSignalEnable::CMD_COMPLETE));
        assert!(v.contains(NormalIntSignalEnable::DATA_COMPLETE));
        assert!(!NormalIntSignalEnable::CMD_COMPLETE.contains(NormalIntSignalEnable::DATA_COMPLETE));
    }

    #[test]
    fn error_int_signal_enable_flag_bits() {
        assert_eq!(ErrorIntSignalEnable::CMD_TIMEOUT.bits(), 1 << 0, "cmd_timeout");
        assert_eq!(ErrorIntSignalEnable::CMD_CRC.bits(), 1 << 1, "cmd_crc");
        assert_eq!(ErrorIntSignalEnable::CMD_END_BIT.bits(), 1 << 2, "cmd_end_bit");
        assert_eq!(ErrorIntSignalEnable::CMD_INDEX.bits(), 1 << 3, "cmd_index");
        assert_eq!(ErrorIntSignalEnable::DATA_TIMEOUT.bits(), 1 << 4, "data_timeout");
        assert_eq!(ErrorIntSignalEnable::DATA_CRC.bits(), 1 << 5, "data_crc");
        assert_eq!(ErrorIntSignalEnable::DATA_END_BIT.bits(), 1 << 6, "data_end_bit");
        assert_eq!(ErrorIntSignalEnable::CURRENT_LIMIT.bits(), 1 << 7, "current_limit");
        assert_eq!(ErrorIntSignalEnable::AUTO_CMD.bits(), 1 << 8, "auto_cmd");
        assert_eq!(ErrorIntSignalEnable::ADMA.bits(), 1 << 9, "adma");
    }

    #[test]
    fn error_int_signal_enable_flag_compose() {
        let v = ErrorIntSignalEnable::CMD_TIMEOUT | ErrorIntSignalEnable::CMD_CRC;
        assert!(v.contains(ErrorIntSignalEnable::CMD_TIMEOUT));
        assert!(v.contains(ErrorIntSignalEnable::CMD_CRC));
        assert!(!ErrorIntSignalEnable::CMD_TIMEOUT.contains(ErrorIntSignalEnable::CMD_CRC));
    }

    #[test]
    fn host_control_field_roundtrip() {
        // bool field `led_on`
        let on = HostControl::default().with_led_on(true);
        assert!(on.led_on());
        let off = on.with_led_on(false);
        assert!(!off.led_on());
        // bool field `dat_4bit`
        let on = HostControl::default().with_dat_4bit(true);
        assert!(on.dat_4bit());
        let off = on.with_dat_4bit(false);
        assert!(!off.dat_4bit());
        // bool field `high_speed`
        let on = HostControl::default().with_high_speed(true);
        assert!(on.high_speed());
        let off = on.with_high_speed(false);
        assert!(!off.high_speed());
        // enum field `dma_sel`
        assert_eq!(HostControl::default().with_dma_sel(HostControlDmaSel::Sdma).dma_sel().unwrap(), HostControlDmaSel::Sdma);
        assert_eq!(HostControl::default().with_dma_sel(HostControlDmaSel::Adma2_32).dma_sel().unwrap(), HostControlDmaSel::Adma2_32);
        assert_eq!(HostControl::default().with_dma_sel(HostControlDmaSel::Adma2_64).dma_sel().unwrap(), HostControlDmaSel::Adma2_64);
    }

    #[test]
    fn host_control_preserves_reserved_bits() {
        let all_ones: u8 = !0;
        // toggle bool `led_on` without disturbing other bits
        let v = HostControl(all_ones).with_led_on(false);
        assert_eq!(v.0 & !HostControl::LED_ON_MASK, all_ones & !HostControl::LED_ON_MASK);
        // toggle bool `dat_4bit` without disturbing other bits
        let v = HostControl(all_ones).with_dat_4bit(false);
        assert_eq!(v.0 & !HostControl::DAT_4BIT_MASK, all_ones & !HostControl::DAT_4BIT_MASK);
        // toggle bool `high_speed` without disturbing other bits
        let v = HostControl(all_ones).with_high_speed(false);
        assert_eq!(v.0 & !HostControl::HIGH_SPEED_MASK, all_ones & !HostControl::HIGH_SPEED_MASK);
        // set enum `dma_sel` to a known variant
        let v = HostControl(all_ones).with_dma_sel(HostControlDmaSel::Sdma);
        assert_eq!(v.0 & !HostControl::DMA_SEL_MASK, all_ones & !HostControl::DMA_SEL_MASK);
    }

    #[test]
    fn host_control_dma_sel_enum_decode() {
        assert_eq!(HostControlDmaSel::from_bits(0x0), Ok(HostControlDmaSel::Sdma));
        assert_eq!(HostControlDmaSel::from_bits(0x2), Ok(HostControlDmaSel::Adma2_32));
        assert_eq!(HostControlDmaSel::from_bits(0x3), Ok(HostControlDmaSel::Adma2_64));
        assert_eq!(HostControlDmaSel::from_bits(0x1), Err(ReservedBits(0x1)));
    }

    #[test]
    fn power_control_field_roundtrip() {
        // bool field `bus_power_on`
        let on = PowerControl::default().with_bus_power_on(true);
        assert!(on.bus_power_on());
        let off = on.with_bus_power_on(false);
        assert!(!off.bus_power_on());
        // enum field `bus_voltage`
        assert_eq!(PowerControl::default().with_bus_voltage(PowerControlBusVoltage::V18).bus_voltage().unwrap(), PowerControlBusVoltage::V18);
        assert_eq!(PowerControl::default().with_bus_voltage(PowerControlBusVoltage::V30).bus_voltage().unwrap(), PowerControlBusVoltage::V30);
        assert_eq!(PowerControl::default().with_bus_voltage(PowerControlBusVoltage::V33).bus_voltage().unwrap(), PowerControlBusVoltage::V33);
    }

    #[test]
    fn power_control_preserves_reserved_bits() {
        let all_ones: u8 = !0;
        // toggle bool `bus_power_on` without disturbing other bits
        let v = PowerControl(all_ones).with_bus_power_on(false);
        assert_eq!(v.0 & !PowerControl::BUS_POWER_ON_MASK, all_ones & !PowerControl::BUS_POWER_ON_MASK);
        // set enum `bus_voltage` to a known variant
        let v = PowerControl(all_ones).with_bus_voltage(PowerControlBusVoltage::V18);
        assert_eq!(v.0 & !PowerControl::BUS_VOLTAGE_MASK, all_ones & !PowerControl::BUS_VOLTAGE_MASK);
    }

    #[test]
    fn power_control_bus_voltage_enum_decode() {
        assert_eq!(PowerControlBusVoltage::from_bits(0x5), Ok(PowerControlBusVoltage::V18));
        assert_eq!(PowerControlBusVoltage::from_bits(0x6), Ok(PowerControlBusVoltage::V30));
        assert_eq!(PowerControlBusVoltage::from_bits(0x7), Ok(PowerControlBusVoltage::V33));
        assert_eq!(PowerControlBusVoltage::from_bits(0x0), Err(ReservedBits(0x0)));
    }

    #[test]
    fn clock_control_field_roundtrip() {
        // bool field `int_clk_en`
        let on = ClockControl::default().with_int_clk_en(true);
        assert!(on.int_clk_en());
        let off = on.with_int_clk_en(false);
        assert!(!off.int_clk_en());
        // RO field `int_clk_stable` — see clock_control_ro_field_read
        // bool field `sd_clk_en`
        let on = ClockControl::default().with_sd_clk_en(true);
        assert!(on.sd_clk_en());
        let off = on.with_sd_clk_en(false);
        assert!(!off.sd_clk_en());
        // scalar field `freq_sel_upper`
        for v in [0 as u16, 1, 0x3] {
            assert_eq!(ClockControl::default().with_freq_sel_upper(v).freq_sel_upper(), v);
        }
        // scalar field `freq_sel`
        for v in [0 as u16, 1, 0xff] {
            assert_eq!(ClockControl::default().with_freq_sel(v).freq_sel(), v);
        }
    }

    #[test]
    fn clock_control_preserves_reserved_bits() {
        let all_ones: u16 = !0;
        // toggle bool `int_clk_en` without disturbing other bits
        let v = ClockControl(all_ones).with_int_clk_en(false);
        assert_eq!(v.0 & !ClockControl::INT_CLK_EN_MASK, all_ones & !ClockControl::INT_CLK_EN_MASK);
        // toggle bool `sd_clk_en` without disturbing other bits
        let v = ClockControl(all_ones).with_sd_clk_en(false);
        assert_eq!(v.0 & !ClockControl::SD_CLK_EN_MASK, all_ones & !ClockControl::SD_CLK_EN_MASK);
        // set scalar `freq_sel_upper` to 0
        let v = ClockControl(all_ones).with_freq_sel_upper(0);
        assert_eq!(v.0 & !ClockControl::FREQ_SEL_UPPER_MASK, all_ones & !ClockControl::FREQ_SEL_UPPER_MASK);
        // set scalar `freq_sel` to 0
        let v = ClockControl(all_ones).with_freq_sel(0);
        assert_eq!(v.0 & !ClockControl::FREQ_SEL_MASK, all_ones & !ClockControl::FREQ_SEL_MASK);
    }

    #[test]
    fn clock_control_ro_field_read() {
        // RO bool `int_clk_stable` — bit 1 reads as the field state
        assert!(ClockControl(0x2u16).int_clk_stable());
        assert!(!ClockControl(!0x2u16).int_clk_stable());
    }

    #[test]
    fn timeout_control_field_roundtrip() {
        // scalar field `dat_timeout_counter`
        for v in [0 as u8, 1, 0xf] {
            assert_eq!(TimeoutControl::default().with_dat_timeout_counter(v).dat_timeout_counter(), v);
        }
    }

    #[test]
    fn timeout_control_preserves_reserved_bits() {
        let all_ones: u8 = !0;
        // set scalar `dat_timeout_counter` to 0
        let v = TimeoutControl(all_ones).with_dat_timeout_counter(0);
        assert_eq!(v.0 & !TimeoutControl::DAT_TIMEOUT_COUNTER_MASK, all_ones & !TimeoutControl::DAT_TIMEOUT_COUNTER_MASK);
    }

    #[test]
    fn capabilities_field_roundtrip() {
        // scalar field `timeout_clock_freq`
        for v in [0 as u32, 1, 0x3f] {
            assert_eq!(Capabilities::default().with_timeout_clock_freq(v).timeout_clock_freq(), v);
        }
        // scalar field `base_clock_mhz`
        for v in [0 as u32, 1, 0xff] {
            assert_eq!(Capabilities::default().with_base_clock_mhz(v).base_clock_mhz(), v);
        }
        // enum field `max_block_len`
        assert_eq!(Capabilities::default().with_max_block_len(CapabilitiesMaxBlockLen::B512).max_block_len().unwrap(), CapabilitiesMaxBlockLen::B512);
        assert_eq!(Capabilities::default().with_max_block_len(CapabilitiesMaxBlockLen::B1024).max_block_len().unwrap(), CapabilitiesMaxBlockLen::B1024);
        assert_eq!(Capabilities::default().with_max_block_len(CapabilitiesMaxBlockLen::B2048).max_block_len().unwrap(), CapabilitiesMaxBlockLen::B2048);
        // bool field `adma2_supported`
        let on = Capabilities::default().with_adma2_supported(true);
        assert!(on.adma2_supported());
        let off = on.with_adma2_supported(false);
        assert!(!off.adma2_supported());
        // bool field `high_speed_supported`
        let on = Capabilities::default().with_high_speed_supported(true);
        assert!(on.high_speed_supported());
        let off = on.with_high_speed_supported(false);
        assert!(!off.high_speed_supported());
        // bool field `voltage_330`
        let on = Capabilities::default().with_voltage_330(true);
        assert!(on.voltage_330());
        let off = on.with_voltage_330(false);
        assert!(!off.voltage_330());
        // bool field `voltage_300`
        let on = Capabilities::default().with_voltage_300(true);
        assert!(on.voltage_300());
        let off = on.with_voltage_300(false);
        assert!(!off.voltage_300());
        // bool field `voltage_180`
        let on = Capabilities::default().with_voltage_180(true);
        assert!(on.voltage_180());
        let off = on.with_voltage_180(false);
        assert!(!off.voltage_180());
    }

    #[test]
    fn capabilities_preserves_reserved_bits() {
        let all_ones: u32 = !0;
        // set scalar `timeout_clock_freq` to 0
        let v = Capabilities(all_ones).with_timeout_clock_freq(0);
        assert_eq!(v.0 & !Capabilities::TIMEOUT_CLOCK_FREQ_MASK, all_ones & !Capabilities::TIMEOUT_CLOCK_FREQ_MASK);
        // set scalar `base_clock_mhz` to 0
        let v = Capabilities(all_ones).with_base_clock_mhz(0);
        assert_eq!(v.0 & !Capabilities::BASE_CLOCK_MHZ_MASK, all_ones & !Capabilities::BASE_CLOCK_MHZ_MASK);
        // set enum `max_block_len` to a known variant
        let v = Capabilities(all_ones).with_max_block_len(CapabilitiesMaxBlockLen::B512);
        assert_eq!(v.0 & !Capabilities::MAX_BLOCK_LEN_MASK, all_ones & !Capabilities::MAX_BLOCK_LEN_MASK);
        // toggle bool `adma2_supported` without disturbing other bits
        let v = Capabilities(all_ones).with_adma2_supported(false);
        assert_eq!(v.0 & !Capabilities::ADMA2_SUPPORTED_MASK, all_ones & !Capabilities::ADMA2_SUPPORTED_MASK);
        // toggle bool `high_speed_supported` without disturbing other bits
        let v = Capabilities(all_ones).with_high_speed_supported(false);
        assert_eq!(v.0 & !Capabilities::HIGH_SPEED_SUPPORTED_MASK, all_ones & !Capabilities::HIGH_SPEED_SUPPORTED_MASK);
        // toggle bool `voltage_330` without disturbing other bits
        let v = Capabilities(all_ones).with_voltage_330(false);
        assert_eq!(v.0 & !Capabilities::VOLTAGE_330_MASK, all_ones & !Capabilities::VOLTAGE_330_MASK);
        // toggle bool `voltage_300` without disturbing other bits
        let v = Capabilities(all_ones).with_voltage_300(false);
        assert_eq!(v.0 & !Capabilities::VOLTAGE_300_MASK, all_ones & !Capabilities::VOLTAGE_300_MASK);
        // toggle bool `voltage_180` without disturbing other bits
        let v = Capabilities(all_ones).with_voltage_180(false);
        assert_eq!(v.0 & !Capabilities::VOLTAGE_180_MASK, all_ones & !Capabilities::VOLTAGE_180_MASK);
    }

    #[test]
    fn capabilities_max_block_len_enum_decode() {
        assert_eq!(CapabilitiesMaxBlockLen::from_bits(0x0), Ok(CapabilitiesMaxBlockLen::B512));
        assert_eq!(CapabilitiesMaxBlockLen::from_bits(0x1), Ok(CapabilitiesMaxBlockLen::B1024));
        assert_eq!(CapabilitiesMaxBlockLen::from_bits(0x2), Ok(CapabilitiesMaxBlockLen::B2048));
        assert_eq!(CapabilitiesMaxBlockLen::from_bits(0x3), Err(ReservedBits(0x3)));
    }

    #[test]
    fn transfer_mode_command_combined_trigger_single_u32_store() {
        let region = MockMmioRegion::for_layout::<Sdhci>();
        let regs = region.as_mapped_regs::<Sdhci>();
        let _tk = __sdhci_internal_mint(regs.regs());
        // Drain any construction-time log entries; the only ops
        // asserted below are the ones the setter call produces.
        let _ = region.take_ops();
        regs.regs().set_transfer_mode_command(TransferMode(0x1234u16), Command(0x1334u16), &_tk);
        let ops = region.take_ops();
        // Single u32 store at the register's offset is the load-
        // bearing property — BCM2711 SDHCI silently drops the
        // command if the write is split into halves. A future
        // emitter regression that emits two stores would produce
        // ops.len() == 2 here.
        assert_eq!(ops.len(), 1, "expected exactly one MMIO op, got {:?}", ops);
        match ops[0] {
            lockjaw_mmio::mock::MockedOp::Write { offset: 0xc, width: 4, value } => {
                assert_eq!(value, 0x13341234u64, "composed value");
            }
            ref op => panic!("expected single u32 Write at 0xc, got {:?}", op),
        }
    }

    #[test]
    fn sysaddr_argument2_alias_round_trips_through_same_cell() {
        let region = MockMmioRegion::for_layout::<Sdhci>();
        let regs = region.as_mapped_regs::<Sdhci>();
        let _tk = __sdhci_internal_mint(regs.regs());
        // Write via the alias setter; peek raw at the primary's struct-field offset.
        regs.regs().write_argument2(0xDEAD_BEEFu32, &_tk);
        assert_eq!(region.peek_u32(offset_of!(Sdhci, sysaddr)), 0xDEAD_BEEFu32);
        // Vice versa: write via the primary, read through the alias getter.
        regs.regs().write_sysaddr(0xCAFE_BABEu32, &_tk);
        assert_eq!(regs.regs().read_argument2(&_tk), 0xCAFE_BABEu32);
    }

    mod _verify {
        use super::*;
        use static_assertions::const_assert_eq;
        const_assert_eq!(
            offset_of!(Sdhci, sysaddr) as u64,
            lockjaw_types::sdhci::SDHCI_SYSADDR
        );
        const_assert_eq!(
            offset_of!(Sdhci, block_size) as u64,
            lockjaw_types::sdhci::SDHCI_BLOCK_SIZE
        );
        const_assert_eq!(
            offset_of!(Sdhci, block_count) as u64,
            lockjaw_types::sdhci::SDHCI_BLOCK_COUNT
        );
        const_assert_eq!(
            offset_of!(Sdhci, argument) as u64,
            lockjaw_types::sdhci::SDHCI_ARGUMENT
        );
        const_assert_eq!(
            offset_of!(Sdhci, transfer_mode_command) as u64,
            lockjaw_types::sdhci::SDHCI_TRANSFER_MODE
        );
        const_assert_eq!(
            offset_of!(Sdhci, response_0) as u64,
            lockjaw_types::sdhci::SDHCI_RESPONSE_0
        );
        const_assert_eq!(
            offset_of!(Sdhci, response_1) as u64,
            lockjaw_types::sdhci::SDHCI_RESPONSE_1
        );
        const_assert_eq!(
            offset_of!(Sdhci, response_2) as u64,
            lockjaw_types::sdhci::SDHCI_RESPONSE_2
        );
        const_assert_eq!(
            offset_of!(Sdhci, response_3) as u64,
            lockjaw_types::sdhci::SDHCI_RESPONSE_3
        );
        const_assert_eq!(
            offset_of!(Sdhci, buffer_data_port) as u64,
            lockjaw_types::sdhci::SDHCI_BUFFER_DATA_PORT
        );
        const_assert_eq!(
            offset_of!(Sdhci, present_state) as u64,
            lockjaw_types::sdhci::SDHCI_PRESENT_STATE
        );
        const_assert_eq!(
            offset_of!(Sdhci, host_control) as u64,
            lockjaw_types::sdhci::SDHCI_HOST_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, power_control) as u64,
            lockjaw_types::sdhci::SDHCI_POWER_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, block_gap_control) as u64,
            lockjaw_types::sdhci::SDHCI_BLOCK_GAP_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, wakeup_control) as u64,
            lockjaw_types::sdhci::SDHCI_WAKEUP_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, clock_control) as u64,
            lockjaw_types::sdhci::SDHCI_CLOCK_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, timeout_control) as u64,
            lockjaw_types::sdhci::SDHCI_TIMEOUT_CONTROL
        );
        const_assert_eq!(
            offset_of!(Sdhci, software_reset) as u64,
            lockjaw_types::sdhci::SDHCI_SOFTWARE_RESET
        );
        const_assert_eq!(
            offset_of!(Sdhci, normal_int_status) as u64,
            lockjaw_types::sdhci::SDHCI_NORMAL_INT_STATUS
        );
        const_assert_eq!(
            offset_of!(Sdhci, error_int_status) as u64,
            lockjaw_types::sdhci::SDHCI_ERROR_INT_STATUS
        );
        const_assert_eq!(
            offset_of!(Sdhci, normal_int_status_enable) as u64,
            lockjaw_types::sdhci::SDHCI_NORMAL_INT_STATUS_ENABLE
        );
        const_assert_eq!(
            offset_of!(Sdhci, error_int_status_enable) as u64,
            lockjaw_types::sdhci::SDHCI_ERROR_INT_STATUS_ENABLE
        );
        const_assert_eq!(
            offset_of!(Sdhci, normal_int_signal_enable) as u64,
            lockjaw_types::sdhci::SDHCI_NORMAL_INT_SIGNAL_ENABLE
        );
        const_assert_eq!(
            offset_of!(Sdhci, error_int_signal_enable) as u64,
            lockjaw_types::sdhci::SDHCI_ERROR_INT_SIGNAL_ENABLE
        );
        const_assert_eq!(
            offset_of!(Sdhci, capabilities) as u64,
            lockjaw_types::sdhci::SDHCI_CAPABILITIES
        );
        const_assert_eq!(
            offset_of!(Sdhci, capabilities_hi) as u64,
            lockjaw_types::sdhci::SDHCI_CAPABILITIES_HI
        );
        const_assert_eq!(
            offset_of!(Sdhci, adma_address) as u64,
            lockjaw_types::sdhci::SDHCI_ADMA_ADDRESS
        );
        const_assert_eq!(
            offset_of!(Sdhci, host_version) as u64,
            lockjaw_types::sdhci::SDHCI_HOST_VERSION
        );
    }

}
