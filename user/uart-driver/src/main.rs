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
    let map_result = sys_map_pages(UART_PHYS, UART_VA, MAP_FLAG_DEVICE);
    if map_result != 0 {
        puts("uart-driver: map MMIO FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }
    puts("uart-driver: MMIO mapped\n");

    // Step 2: Create a notification for the UART RX interrupt
    let notif_ps = sys_alloc_pages(1);
    let notif_handle = sys_create_notification(notif_ps);
    puts("uart-driver: notification created\n");

    // Step 3: Bind UART1 IRQ (INTID 40) to the notification
    let bind_result = sys_bind_irq(UART_INTID, notif_handle);
    if bind_result != 0 {
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

    // Step 5: Polling server loop.
    // Handle 0 = endpoint (copied from parent at process creation).
    // Check both RX (via MMIO) and TX requests (via non-blocking IPC) each round.
    loop {
        unsafe {
            // Check RX directly via MMIO — drain up to 16 bytes per poll
            let mut rx_count = 0;
            while uart_read32(UARTFR) & UARTFR_RXFE == 0 && rx_count < 16 {
                let ch = (uart_read32(UARTDR) & 0xFF) as u8;
                // Echo the character back
                uart_putc(ch);
                // Echo newline as \r\n
                if ch == b'\r' {
                    uart_putc(b'\n');
                }
                rx_count += 1;
            }
        }

        // Check for TX requests (non-blocking IPC receive on handle 0)
        let result = sys_recv_nb(0);
        if result != SYS_ERR_WOULD_BLOCK {
            // Got a message — first word is the character to print
            unsafe { uart_putc(result as u8); }
            sys_reply(0, 0, 0, 0, 0);
        }

        sys_yield();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("uart-driver: PANIC\n");
    loop {
        unsafe { asm!("wfi"); }
    }
}
