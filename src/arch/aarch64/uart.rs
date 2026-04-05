use core::ptr;

/// PL011 UART0 base address on QEMU virt machine.
const UART0_BASE: usize = 0x0900_0000;

/// Data Register — write a byte here to transmit.
const UARTDR: usize = UART0_BASE + 0x00;

/// Flag Register — bit 5 (TXFF) indicates TX FIFO is full.
const UARTFR: usize = UART0_BASE + 0x18;

/// TX FIFO Full flag in UARTFR.
const UARTFR_TXFF: u32 = 1 << 5;

/// PL011 UART driver — zero-sized, hardcoded MMIO addresses.
///
/// Concrete type (not behind `dyn Trait`) so `cargo-call-stack` can
/// trace the full call graph through formatting code.
pub struct Uart;

impl Uart {
    pub const fn new() -> Self {
        Uart
    }

    /// Transmit a single byte, blocking until the TX FIFO has space.
    pub fn putc(&self, c: u8) {
        unsafe {
            // Spin while TX FIFO is full
            while (ptr::read_volatile(UARTFR as *const u32) & UARTFR_TXFF) != 0 {
                core::hint::spin_loop();
            }
            // Write the byte to the data register
            ptr::write_volatile(UARTDR as *mut u32, c as u32);
        }
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
