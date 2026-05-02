use core::cell::UnsafeCell;
use core::ptr;

/// TX FIFO Full flag in UARTFR.
const UARTFR_TXFF: u32 = 1 << 5;

/// Offset of Data Register from base.
const DR_OFFSET: usize = 0x00;

/// Offset of Flag Register from base.
const FR_OFFSET: usize = 0x18;

/// Offset of Integer Baud Rate Divisor register from base.
const IBRD_OFFSET: usize = 0x24;

/// Offset of Fractional Baud Rate Divisor register from base.
const FBRD_OFFSET: usize = 0x28;

/// Offset of Line Control Register from base.
const LCR_H_OFFSET: usize = 0x2C;

/// Offset of Control Register from base.
const CR_OFFSET: usize = 0x30;

/// Singleton holding the UART base address. Zero until set_base() is
/// called after DTB discovery. Switches to the higher-half VA after
/// `use_high_addresses()` is called.
struct UartBase(UnsafeCell<usize>);

/// SAFETY: single-core kernel. The base address is written once during
/// boot (set_base), then only read. No concurrent access.
unsafe impl Sync for UartBase {}

static UART: UartBase = UartBase(UnsafeCell::new(0));

/// PL011 UART driver — zero-sized, hardcoded MMIO addresses.
///
/// Concrete type (not behind `dyn Trait`) so `cargo-call-stack` can
/// trace the full call graph through formatting code.
pub struct Uart;

impl Uart {
    /// Create a new UART handle. Zero-sized; all state is in MMIO registers.
    pub const fn new() -> Self {
        Uart
    }

    /// Transmit a single byte, blocking until the TX FIFO has space.
    pub fn putc(&self, c: u8) {
        // SAFETY: single-core; UART base set at boot and never changes after.
        let base = unsafe { *UART.0.get() };
        // Calling putc before set_base() is a boot-order bug, not something
        // to hide with a silent no-op. In release, trust boot ordering
        // (discover halts before any print if UART missing).
        debug_assert!(base != 0, "UART putc called before set_base()");
        unsafe {
            // Spin while TX FIFO is full
            // SAFETY: MMIO address — FR register
            while (ptr::read_volatile((base + FR_OFFSET) as *const u32) & UARTFR_TXFF) != 0 {
                core::hint::spin_loop();
            }
            // Write the byte to the data register
            // SAFETY: MMIO address — DR register
            ptr::write_volatile((base + DR_OFFSET) as *mut u32, c as u32);
        }
    }

    /// Update the UART base address from DTB discovery.
    /// Called after platform::discover(), before any prints.
    ///
    /// # Safety
    /// Must be called during single-core boot, before any concurrent access.
    pub unsafe fn set_base(phys_addr: u64) {
        *UART.0.get() = phys_addr as usize;
    }

    /// Program the PL011 baud rate to 115200.
    ///
    /// Pi 4B firmware sometimes hands off with the wrong baud rate
    /// (103448 instead of 115200). This sets IBRD/FBRD explicitly.
    /// UARTCLK = 48 MHz: 48000000 / (16 * 115200) = 26.0416...
    ///   IBRD = 26, FBRD = round(0.0416 * 64 + 0.5) = 3
    ///
    /// Called unconditionally — QEMU's virtual PL011 tolerates baud
    /// register writes, and Pi 4B needs them.
    ///
    /// # Safety
    /// Must be called during single-core boot with UART base already set.
    pub unsafe fn init_baud(&self) {
        let base = *UART.0.get();
        // Disable UART while changing baud rate
        // SAFETY: MMIO address — CR register
        let cr = ptr::read_volatile((base + CR_OFFSET) as *const u32);
        ptr::write_volatile((base + CR_OFFSET) as *mut u32, 0);

        // Set baud rate: 48MHz / (16 * 115200) → IBRD=26, FBRD=3
        // SAFETY: MMIO address — IBRD register
        ptr::write_volatile((base + IBRD_OFFSET) as *mut u32, 26);
        // SAFETY: MMIO address — FBRD register
        ptr::write_volatile((base + FBRD_OFFSET) as *mut u32, 3);

        // 8N1: 8 data bits, no parity, 1 stop bit, enable FIFOs
        // SAFETY: MMIO address — LCR_H register
        ptr::write_volatile((base + LCR_H_OFFSET) as *mut u32, (3 << 5) | (1 << 4));

        // Re-enable UART (TX + RX + UART enable)
        // SAFETY: MMIO address — CR register
        ptr::write_volatile((base + CR_OFFSET) as *mut u32, cr | (1 << 0) | (1 << 8) | (1 << 9));
    }

    /// Switch UART to higher-half virtual addresses.
    ///
    /// # Safety
    /// Higher-half mapping must be active (TTBR1 installed).
    pub unsafe fn use_high_addresses() {
        let current = *UART.0.get();
        *UART.0.get() = current + crate::mm::addr::KERNEL_VA_OFFSET as usize;
    }

    /// Transmit a string, converting `\n` to `\r\n` for serial terminals.
    pub fn puts(&self, s: &str) {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.putc(b'\r');
            }
            self.putc(byte);
        }
    }
}
