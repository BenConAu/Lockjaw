#![no_std]
#![no_main]

use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;

// ---------------------------------------------------------------------------
// PL011 UART register offsets
// ---------------------------------------------------------------------------

const UARTDR: u64 = 0x00;    // Data register (read = RX, write = TX)
const UARTFR: u64 = 0x18;    // Flag register
const UARTIMSC: u64 = 0x38;  // Interrupt mask set/clear

const UARTFR_TXFF: u32 = 1 << 5;  // TX FIFO full
const UARTFR_RXFE: u32 = 1 << 4;  // RX FIFO empty
const UARTIMSC_RXIM: u32 = 1 << 4; // RX interrupt mask

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// User VA where we map the UART MMIO page. L2[1] (0x0020_0000) avoids
/// conflict with L2[4] (kernel device MMIO block) and L2[2] (user code).
const UART_VA: u64 = 0x0020_0000;

/// PL011 UART1 physical address on QEMU virt (from DTB: pl011@9040000).
/// UART0 (0x0900_0000) stays with the kernel for kprintln/debug.
const UART_PHYS: u64 = 0x0904_0000;

/// UART1 SPI interrupt: SPI 8 = INTID 40 on QEMU virt (from DTB: interrupts <0 8 4>).
const UART_INTID: u64 = 40;

// ---------------------------------------------------------------------------
// UART MMIO helpers
// ---------------------------------------------------------------------------

unsafe fn uart_read32(offset: u64) -> u32 {
    ptr::read_volatile((UART_VA + offset) as *const u32)
}

unsafe fn uart_write32(offset: u64, val: u32) {
    ptr::write_volatile((UART_VA + offset) as *mut u32, val);
}

/// Write a byte to the UART, spinning while TX FIFO is full.
unsafe fn uart_putc(c: u8) {
    while uart_read32(UARTFR) & UARTFR_TXFF != 0 {
        core::hint::spin_loop();
    }
    uart_write32(UARTDR, c as u32);
}

/// Write a string to the UART, converting \n to \r\n.
unsafe fn uart_puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            uart_putc(b'\r');
        }
        uart_putc(b);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Print early banner via sys_debug_putc (kernel UART, known to work)
    puts("uart-driver: starting\n");

    // Step 1: Map UART1 MMIO page into our address space
    if !sys_map_pages(UART_PHYS, UART_VA, MAP_FLAG_DEVICE).is_ok() {
        puts("uart-driver: map MMIO FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }
    puts("uart-driver: MMIO mapped\n");

    // Step 2: Create a notification for the UART RX interrupt
    let notif_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("uart-driver: alloc FAILED\n"); loop { unsafe { asm!("wfi"); } } }
    };
    let notif_handle = match sys_create_notification(notif_ps) {
        Ok(h) => h,
        Err(_) => { puts("uart-driver: create notif FAILED\n"); loop { unsafe { asm!("wfi"); } } }
    };
    puts("uart-driver: notification created\n");

    // Step 3: Bind UART1 IRQ (INTID 40) to the notification
    if !sys_bind_irq(UART_INTID, notif_handle).is_ok() {
        puts("uart-driver: bind IRQ FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }
    puts("uart-driver: IRQ bound\n");

    // Step 4: Enable PL011 RX interrupt via mapped MMIO
    unsafe {
        let imsc = uart_read32(UARTIMSC);
        uart_write32(UARTIMSC, imsc | UARTIMSC_RXIM);
    }

    // Print banner via UART1 (our own mapped UART)
    unsafe { uart_puts("uart-driver: UART1 active\n"); }
    // Also confirm via kernel UART0
    puts("uart-driver: server ready\n");

    // Step 5: Event-driven server loop using sys_wait_any.
    // Handle 0 = endpoint (copied from parent at process creation).
    // Handle at notif_handle = notification (created above, bound to UART1 IRQ).
    // The thread sleeps until either an IPC message or an IRQ arrives.
    let mut irq_threshold: u64 = 1;
    let mut entries = [
        WaitEntry { handle: 0, threshold: 0 },                       // endpoint
        WaitEntry { handle: notif_handle, threshold: irq_threshold }, // notification
    ];

    loop {
        let mask = match sys_wait_any(&entries) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Bit 0: endpoint ready — IPC TX request
        if mask & 1 != 0 {
            if let Ok(ch) = sys_receive(0) {
                unsafe { uart_putc(ch as u8); }
            }
            sys_reply(0, 0, 0, 0, 0);
        }

        // Bit 1: notification ready — UART RX interrupt fired
        if mask & 2 != 0 {
            unsafe {
                // Drain the RX FIFO
                while uart_read32(UARTFR) & UARTFR_RXFE == 0 {
                    let ch = (uart_read32(UARTDR) & 0xFF) as u8;
                    // Echo the character back
                    uart_putc(ch);
                    // Echo newline as \r\n
                    if ch == b'\r' {
                        uart_putc(b'\n');
                    }
                }
            }
            // Advance threshold for the next IRQ
            irq_threshold += 1;
            entries[1].threshold = irq_threshold;
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("uart-driver: PANIC\n");
    loop {
        unsafe { asm!("wfi"); }
    }
}
