#![no_std]
#![no_main]

use core::arch::asm;
use core::ptr;

// ---------------------------------------------------------------------------
// PL011 UART register offsets
// ---------------------------------------------------------------------------

const UARTDR: u64 = 0x00;    // Data register (read = RX, write = TX)
const UARTFR: u64 = 0x18;    // Flag register
const UARTIMSC: u64 = 0x38;  // Interrupt mask set/clear
const UARTICR: u64 = 0x44;   // Interrupt clear register

const UARTFR_TXFF: u32 = 1 << 5;  // TX FIFO full
const UARTFR_RXFE: u32 = 1 << 4;  // RX FIFO empty
const UARTIMSC_RXIM: u32 = 1 << 4; // RX interrupt mask

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// User VA where we map the UART MMIO page. L2[1] (0x0020_0000) avoids
/// conflict with L2[4] (kernel device MMIO block) and L2[2] (user code).
const UART_VA: u64 = 0x0020_0000;

/// PL011 UART0 physical address on QEMU virt.
const UART_PHYS: u64 = 0x0900_0000;

/// UART0 SPI interrupt: SPI 1 = INTID 33 on QEMU virt.
const UART_INTID: u64 = 33;

/// MAP_FLAG_DEVICE — must match lockjaw-types::vmem::MAP_FLAG_DEVICE.
const MAP_FLAG_DEVICE: u64 = 1 << 0;

// ---------------------------------------------------------------------------
// Syscall wrappers
// ---------------------------------------------------------------------------

fn putc(c: u8) {
    unsafe {
        asm!("svc #0", in("x0") c as u64, in("x8") 0u64);
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn sys_alloc_pages(count: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") count, in("x8") 6u64, lateout("x0") result);
    }
    result
}

fn sys_map_pages(x0: u64, virt_addr: u64, flags: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") x0,
            in("x1") virt_addr,
            in("x2") flags,
            in("x8") 7u64,
            lateout("x0") result,
        );
    }
    result
}

fn sys_create_notification(pageset_id: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") pageset_id, in("x8") 9u64, lateout("x0") result);
    }
    result
}

fn sys_wait_notification(handle: u64, threshold: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") handle,
            in("x1") threshold,
            in("x8") 11u64,
            lateout("x0") result,
        );
    }
    result
}

fn sys_bind_irq(intid: u64, notif_handle: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") intid,
            in("x1") notif_handle,
            in("x8") 12u64,
            lateout("x0") result,
        );
    }
    result
}

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

    // Step 1: Map UART MMIO page into our address space
    let map_result = sys_map_pages(UART_PHYS, UART_VA, MAP_FLAG_DEVICE);
    if map_result != 0 {
        puts("uart-driver: map UART MMIO FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }

    // Step 2: Create a notification for the UART RX interrupt
    let notif_ps = sys_alloc_pages(1);
    if notif_ps == u64::MAX {
        puts("uart-driver: alloc notification page FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }

    let notif_handle = sys_create_notification(notif_ps);
    if notif_handle >= 256 {
        // Handle indices are small; large values indicate error codes
        puts("uart-driver: create notification FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }

    // Step 3: Bind UART IRQ (INTID 33) to the notification
    let bind_result = sys_bind_irq(UART_INTID, notif_handle);
    if bind_result != 0 {
        puts("uart-driver: bind IRQ FAILED\n");
        loop { unsafe { asm!("wfi"); } }
    }

    // Step 4: Enable PL011 RX interrupt via mapped MMIO
    unsafe {
        let imsc = uart_read32(UARTIMSC);
        uart_write32(UARTIMSC, imsc | UARTIMSC_RXIM);
    }

    // Print banner via our own mapped UART
    unsafe { uart_puts("uart-driver: UART mapped, IRQ bound, RX echo active\n"); }

    // Step 5: Main loop — wait for RX interrupt, read and echo characters
    let mut threshold: u64 = 1;
    loop {
        sys_wait_notification(notif_handle, threshold);
        threshold += 1;

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

            // Clear the RX interrupt
            uart_write32(UARTICR, UARTIMSC_RXIM);
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
