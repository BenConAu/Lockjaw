#![no_std]
#![no_main]
// Driver-crate body writes zero `unsafe` blocks AND zero
// `#[allow(unsafe_code)]` attributes. The macro-generated boot
// stubs in `lockjaw_userlib::boot_stub!` are the single audited
// location for the boot-entry attributes; the macro's expansion is
// the only place `#[allow(unsafe_code)]` appears for this build.
//
// `#![deny]` (not `#![forbid]`) so the macro-emitted per-item
// allows on `#[no_mangle]` and `#[link_section]` are honoured.
// Acceptance grep:
// `grep -rn 'allow(unsafe_code)' user/uart-driver/src/`
// MUST return nothing.
#![deny(unsafe_code)]

use lockjaw_userlib::driver_runtime::{run_event_server, DriverCtx, EventEngine};
use lockjaw_userlib::pl011::{
    drain_rx_fifo, set_interrupt_masks, write_byte_deadline, Imsc, Pl011,
};
use lockjaw_userlib::time::{cntfreq_hz, monotonic_now, Nanos};
use lockjaw_userlib::{driver_main, puts, sys_exit, PL011_HASH};
use lockjaw_mmio::region::MappedRegs;

// Per-board TX-spin deadline. 10 ms ≈ 115 byte-times at 115200 baud
// — well over any transient FIFO stall, well under human-perceptible
// hang. Tunable per-board without touching lockjaw-userlib. If QEMU CI
// surfaces a flake on a loaded host, raise to 50 ms or 100 ms; if a
// real board exhibits hung-FIFO behavior, the 10 ms ceiling is what
// proves the rewrite worked (driver continues instead of hangs).
const TX_TIMEOUT_NANOS: Nanos = Nanos(10_000_000);

// ---------------------------------------------------------------------------
// PL011 helpers (the only "device behaviour" the driver expresses —
// the rest of the register surface is the generated lockjaw_regs::pl011,
// reached through lockjaw_userlib::pl011's curated re-export; drivers
// cannot name lockjaw_regs directly per check-driver-unsafe).
// ---------------------------------------------------------------------------

/// Write a byte to the UART, bounded by `TX_TIMEOUT_NANOS`. On
/// timeout the byte is dropped (no error propagation path exists at
/// the current callsites — banner / IPC TX / IRQ echo all discard
/// the result). The visible improvement over the pre-P2 unbounded
/// spin is that the driver cannot infinite-loop on a stuck FIFO.
fn uart_putc(regs: &Pl011, c: u8) {
    let deadline = monotonic_now().deadline_in(TX_TIMEOUT_NANOS, cntfreq_hz());
    let _ = write_byte_deadline(regs, c, deadline);
}

/// Write a string to the UART, converting `\n` to `\r\n` (PL011 expects
/// the explicit CR for terminals that don't auto-translate).
fn uart_puts(regs: &Pl011, s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            uart_putc(regs, b'\r');
        }
        uart_putc(regs, b);
    }
}

// ---------------------------------------------------------------------------
// Event engine — implements the canonical event-loop driver shape.
// Owns the typed MMIO region; on_ipc forwards a byte to TX, on_irq
// drains the RX FIFO. The framework owns the sys_wait_any loop, the
// IRQ-threshold bookkeeping, the IPC receive/reply syscalls, and the
// bit-mask constants — this driver has ZERO raw `sys_*` calls outside
// `sys_exit` in the panic handler.
// ---------------------------------------------------------------------------

struct UartEngine {
    regs: MappedRegs<Pl011>,
}

impl UartEngine {
    fn regs(&self) -> &Pl011 { self.regs.regs() }
}

impl EventEngine for UartEngine {
    fn on_ipc(&mut self, msg: u64) -> u64 {
        // IPC TX request: the message word is the byte to send.
        uart_putc(self.regs(), msg as u8);
        0
    }

    fn on_irq(&mut self) {
        // Drain the RX FIFO; echo each character (CR also produces LF).
        // Framework `drain_rx_fifo` owns the FIFO-empty check; the
        // closure decides what to do with each byte.
        let regs = self.regs();
        drain_rx_fifo(regs, |ch| {
            uart_putc(regs, ch);
            if ch == b'\r' {
                uart_putc(regs, b'\n');
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Driver main — invoked by the driver_main! macro after boot, probe,
// claim, IRQ bind. Enables PL011's RX interrupt, prints a UART1 banner,
// then hands off to `run_event_server`.
// ---------------------------------------------------------------------------

fn uart_main(ctx: DriverCtx<Pl011>) -> ! {
    let mut engine = UartEngine { regs: ctx.regs };

    // Enable PL011 RX interrupt via write-replace (not RMW): the
    // framework helper writes the full intended mask, clobbering
    // any prior IMSC contents. TXIM clears as deliberate current
    // policy — driver does TX via polling write_byte_deadline, not
    // via TX interrupt. The non-atomic RMW race window closes by
    // construction because there is no read step.
    set_interrupt_masks(engine.regs(), Imsc::RXIM);

    // Kernel-debug-channel confirmation FIRST so the "reached here"
    // signal lands even if UART1 itself is broken — `puts` routes
    // through `sys_debug_puts`, independent of the user PL011.
    // The UART1 banner follows via `uart_putc`, which since P2 is
    // bounded by `TX_TIMEOUT_NANOS` (10 ms per byte): a stuck FIFO
    // drops bytes instead of hanging the driver.
    puts("uart-driver: server ready\n");
    uart_puts(engine.regs(), "uart-driver: UART1 active\n");

    run_event_server(
        &mut engine,
        ctx.server_ep,
        ctx.irq_notif,
        ctx.irq_initial_threshold,
    )
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("uart-driver: PANIC\n");
    sys_exit();
}

// ---------------------------------------------------------------------------
// Driver boot — generated by the macro. The macro's expansion site
// is the single location where the driver build carries
// `#[allow(unsafe_code)]`; the driver crate body is unsafe-free.
// ---------------------------------------------------------------------------

driver_main! {
    name = "uart-driver",
    hash = LOCKJAW_SOURCE_HASH,
    probe_hash = PL011_HASH,
    layout = Pl011,
    main = uart_main,
}
