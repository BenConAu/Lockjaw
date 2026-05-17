#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
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


// ---------------------------------------------------------------------------
// UART MMIO helpers
// ---------------------------------------------------------------------------

unsafe fn uart_read32(base: u64, offset: u64) -> u32 {
    ptr::read_volatile((base + offset) as *const u32)
}

unsafe fn uart_write32(base: u64, offset: u64, val: u32) {
    ptr::write_volatile((base + offset) as *mut u32, val);
}

/// Write a byte to the UART, spinning while TX FIFO is full.
unsafe fn uart_putc(base: u64, c: u8) {
    while uart_read32(base, UARTFR) & UARTFR_TXFF != 0 {
        core::hint::spin_loop();
    }
    uart_write32(base, UARTDR, c as u32);
}

/// Write a string to the UART, converting \n to \r\n.
unsafe fn uart_puts(base: u64, s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            uart_putc(base, b'\r');
        }
        uart_putc(base, b);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Print early banner via sys_debug_putc (kernel UART, known to work)
    puts("uart-driver: starting\n");

    // Allocate our Reply object for outbound sys_call (bootstrap + claim).
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("uart-driver: create reply FAILED\n"); halt() }
    };

    // Bootstrap: call init on handle 0 to receive our handles.
    puts("uart-driver: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("uart-driver: bootstrap FAILED\n"); halt() }
    };
    // Bootstrap reply words are raw handle indices exported by init.
    let uart_srv_ep = EndpointHandle(reply[0]);
    let devmgr_client = EndpointHandle(reply[1]);
    puts("uart-driver: bootstrapped\n");

    // Claim a PL011 device from the device manager.
    let claim = match sys_call_ret4(devmgr_client, reply_obj, CMD_CLAIM_DEVICE, PL011_HASH, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("uart-driver: claim call FAILED\n"); halt() }
    };
    if claim[0] != CLAIM_OK {
        puts("uart-driver: no PL011 available\n");
        halt()
    }
    let mmio_pageset = PageSetHandle(claim[1]);
    let uart_intid = claim[2];
    puts("uart-driver: claimed PL011\n");

    // Step 1: Map UART MMIO page into our address space
    let uart_va = VMEM.alloc(1).expect("VA exhausted for UART MMIO");
    if !sys_map_pages(mmio_pageset, uart_va, MapMemoryAttribute::Device).is_ok() {
        puts("uart-driver: map MMIO FAILED\n");
        halt()
    }
    puts("uart-driver: MMIO mapped\n");

    // Step 2: Create a notification for the UART RX interrupt
    let notif_handle = match sys_alloc_pages(1).and_then(sys_create_notification) {
        Ok(h) => h,
        Err(_) => { puts("uart-driver: create notif FAILED\n"); halt() }
    };
    puts("uart-driver: notification created\n");

    // Step 3: Bind UART IRQ to the notification
    if !sys_bind_irq(uart_intid, notif_handle).is_ok() {
        puts("uart-driver: bind IRQ FAILED\n");
        halt()
    }
    puts("uart-driver: IRQ bound\n");

    // Step 4: Enable PL011 RX interrupt via mapped MMIO
    unsafe {
        let imsc = uart_read32(uart_va, UARTIMSC);
        uart_write32(uart_va, UARTIMSC, imsc | UARTIMSC_RXIM);
    }

    // Print banner via UART1 (our own mapped UART)
    unsafe { uart_puts(uart_va, "uart-driver: UART1 active\n"); }
    // Also confirm via kernel UART0
    puts("uart-driver: server ready\n");

    // Step 5: Event-driven server loop using sys_wait_any.
    // uart_srv_ep = IPC endpoint for character requests from init.
    // notif_handle = notification bound to the UART RX IRQ.
    // The thread sleeps until either an IPC message or an IRQ arrives.
    // WaitEntry.handle is raw u64 because wait_any accepts mixed types.
    let mut irq_threshold: u64 = 1;
    let mut entries = [
        WaitEntry { handle: uart_srv_ep.0, threshold: 0 },
        WaitEntry { handle: notif_handle.0, threshold: irq_threshold },
    ];

    loop {
        let mask = match sys_wait_any(&entries) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Bit 0: endpoint ready — IPC TX request
        if mask & 1 != 0 {
            if let Ok(ch) = sys_receive(uart_srv_ep) {
                unsafe { uart_putc(uart_va, ch as u8); }
            }
            sys_reply(0, 0, 0, 0);
        }

        // Bit 1: notification ready — UART RX interrupt fired
        if mask & 2 != 0 {
            unsafe {
                // Drain the RX FIFO
                while uart_read32(uart_va, UARTFR) & UARTFR_RXFE == 0 {
                    let ch = (uart_read32(uart_va, UARTDR) & 0xFF) as u8;
                    // Echo the character back
                    uart_putc(uart_va, ch);
                    // Echo newline as \r\n
                    if ch == b'\r' {
                        uart_putc(uart_va, b'\n');
                    }
                }
            }
            // Advance threshold for the next IRQ
            irq_threshold += 1;
            entries[1].threshold = irq_threshold;
        }
    }
}

/// Terminate the process. EL0 `wfi`-loops keep the thread `Running`
/// from the scheduler's POV — they don't block; they spin a
/// tick-period each iteration. Use sys_exit so the scheduler removes
/// us from rotation.
fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("uart-driver: PANIC\n");
    sys_exit();
}
