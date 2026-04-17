use core::cell::UnsafeCell;
use core::ptr;

/// PL011 UART0 physical base address (from platform constants).
const UART0_BASE_PHYS: usize = super::platform::UART0_BASE_PHYS as usize;

/// TX FIFO Full flag in UARTFR.
const UARTFR_TXFF: u32 = 1 << 5;

/// Offset of Data Register from base.
const DR_OFFSET: usize = 0x00;

/// Offset of Flag Register from base.
const FR_OFFSET: usize = 0x18;

/// Singleton holding the UART base address. Starts at the physical address
/// (identity-mapped at boot), switches to the higher-half VA after
/// `use_high_addresses()` is called.
struct UartBase(UnsafeCell<usize>);

/// SAFETY: single-core kernel. The base address is written once during
/// boot (use_high_addresses), then only read. No concurrent access.
unsafe impl Sync for UartBase {}

static UART: UartBase = UartBase(UnsafeCell::new(UART0_BASE_PHYS));

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

    /// Switch UART to higher-half virtual addresses.
    ///
    /// # Safety
    /// Higher-half mapping must be active (TTBR1 installed).
    pub unsafe fn use_high_addresses() {
        *UART.0.get() = UART0_BASE_PHYS + crate::mm::addr::KERNEL_VA_OFFSET as usize;
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

impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.puts(s);
        Ok(())
    }
}
