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

    // Bootstrap: call init on handle 0 to receive our handles.
    puts("uart-driver: bootstrapping...\n");
    let reply = match sys_call_ret4(0, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("uart-driver: bootstrap FAILED\n"); loop { unsafe { asm!("wfi"); } } }
    };
    let uart_srv_ep = reply[0];    // IPC server endpoint (character requests from init)
    let devmgr_client = reply[1];  // device-manager client endpoint
    puts("uart-driver: bootstrapped\n");

    // Claim a PL011 device from the device manager.
    let claim = match sys_call_ret4(devmgr_client, CMD_CLAIM_DEVICE, PL011_HASH, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("uart-driver: claim call FAILED\n"); loop { unsafe { asm!("wfi"); } } }
    };
    let uart_phys = claim[0];
    let uart_intid = claim[1];
    if uart_phys == 0 {
        puts("uart-driver: no PL011 available\n");
        loop { unsafe { asm!("wfi"); } }
    }
    puts("uart-driver: claimed PL011\n");

    // Step 1: Map UART MMIO page into our address space
    if !sys_map_pages(uart_phys, UART_VA, MAP_FLAG_DEVICE).is_ok() {
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

    // Step 3: Bind UART IRQ to the notification
    if !sys_bind_irq(uart_intid, notif_handle).is_ok() {
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
    // uart_srv_ep = IPC endpoint for character requests from init.
    // notif_handle = notification bound to the UART RX IRQ.
    // The thread sleeps until either an IPC message or an IRQ arrives.
    let mut irq_threshold: u64 = 1;
    let mut entries = [
        WaitEntry { handle: uart_srv_ep, threshold: 0 },                       // endpoint
        WaitEntry { handle: notif_handle, threshold: irq_threshold },           // notification
    ];

    loop {
        let mask = match sys_wait_any(&entries) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Bit 0: endpoint ready — IPC TX request
        if mask & 1 != 0 {
            if let Ok(ch) = sys_receive(uart_srv_ep) {
                unsafe { uart_putc(ch as u8); }
            }
            sys_reply(uart_srv_ep, 0, 0, 0, 0);
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
