#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;
use lockjaw_types::clock::{
    ClockError,
    CLOCK_OK, CLOCK_ERR_NOT_SUPPORTED,
    CLOCK_OP_SET_RATE, CLOCK_OP_GET_RATE, CLOCK_OP_ENABLE, CLOCK_OP_DISABLE,
    cprman::*,
};
use lockjaw_types::device::BCM2711_CPRMAN_HASH;

// ---------------------------------------------------------------------------
// MMIO helpers
// ---------------------------------------------------------------------------

/// Strip the password byte from a value about to be written to a CM_*
/// or A2W_* register and OR in the canonical password. Every CPRMAN
/// register write must carry CM_PASSWORD in bits[31:24] or the write
/// is silently ignored by the hardware.
fn pwd(value: u32) -> u32 {
    (value & 0x00FF_FFFF) | (CM_PASSWORD << 24)
}

unsafe fn cm_read(base: u64, offset: usize) -> u32 {
    ptr::read_volatile((base + offset as u64) as *const u32)
}

unsafe fn cm_write(base: u64, offset: usize, value: u32) {
    ptr::write_volatile((base + offset as u64) as *mut u32, value);
}

/// Wait for the BUSY bit to clear in CM_*CTL after a write that
/// changes a divider or source. Bounded spin to avoid hanging
/// forever if the hardware never settles; returns Err(Hardware)
/// on timeout.
unsafe fn wait_not_busy(base: u64, ctl_offset: usize) -> Result<(), ClockError> {
    for _ in 0..1_000_000 {
        if cm_read(base, ctl_offset) & CM_CTL_BUSY == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(ClockError::Hardware)
}

// ---------------------------------------------------------------------------
// EMMC2 leaf operations
// ---------------------------------------------------------------------------

/// Set the EMMC2 clock to `target_hz` (computed against PLLD_PER_CORE).
/// Disables, programs the divider + source, re-enables. Returns the
/// actual rate the hardware will produce (may differ from target due
/// to divider quantization — see `compute_divider`).
unsafe fn emmc2_set_rate(base: u64, target_hz: u64) -> Result<u64, ClockError> {
    let (divider, actual_hz) = compute_divider(PLLD_PER_CORE_HZ, target_hz)?;

    // 1. Disable the gate (clear ENABLE) and assert KILL to stop the
    //    output cleanly before changing the divider. Per the BCM
    //    binding you must not change DIV while the clock is running.
    cm_write(base, CM_EMMC2CTL,
        pwd(CM_CTL_KILL | (CM_SRC_PLLD_PER_CORE << CM_CTL_SRC_SHIFT)));
    // Wait for the clock generator to actually stop (BUSY clears once
    // the kill takes effect). Only this transition needs a wait — see
    // step 3.
    wait_not_busy(base, CM_EMMC2CTL)?;

    // 2. Program the new divider while the clock is stopped.
    cm_write(base, CM_EMMC2DIV, pwd(divider));

    // 3. Re-enable: clear KILL, set ENABLE, keep SRC. Linux's
    //    bcm2835_clock_on (clk-bcm2835.c) writes ENABLE and returns
    //    immediately — it does not wait. The hardware sets BUSY once
    //    the generator starts running, which is the *opposite*
    //    transition from what wait_not_busy() polls for, so polling
    //    here would either time out (clock running, BUSY stays set)
    //    or return immediately on a transient (false success). The
    //    write itself is enough.
    cm_write(base, CM_EMMC2CTL,
        pwd(CM_CTL_ENABLE | (CM_SRC_PLLD_PER_CORE << CM_CTL_SRC_SHIFT)));

    Ok(actual_hz)
}

/// Read the current EMMC2 output rate from the divider register.
/// Called from the M0c IPC dispatch path (`dispatch_emmc2`).
unsafe fn emmc2_get_rate(base: u64) -> Result<u64, ClockError> {
    let divider = cm_read(base, CM_EMMC2DIV) & 0xFF_FFFF;
    if divider == 0 {
        return Err(ClockError::OutOfRange);
    }
    Ok((PLLD_PER_CORE_HZ * 4096 + (divider as u64 / 2)) / (divider as u64))
}

// ---------------------------------------------------------------------------
// Self-test (M0b success-line emitter)
// ---------------------------------------------------------------------------

/// Drive the EMMC2 leaf through set_rate(200 MHz) → get_rate, and
/// exercise the NotSupported path on UART (id 19, the BCM2711 binding's
/// CM_UART). Prints the three success lines from the M0b plan.
unsafe fn self_test(base: u64) {
    puts("[CPRMAN] init: register region mapped, taking ownership\n");

    match emmc2_set_rate(base, 200_000_000) {
        Ok(actual) => {
            puts("[CPRMAN] EMMC2 set_rate(200_000_000) -> actual=");
            put_decimal(actual);
            puts(" enabled=1\n");
        }
        Err(_) => {
            puts("[CPRMAN] EMMC2 set_rate FAILED\n");
        }
    }

    // BCM2711 CM_UART id = 19 per the binding; not implemented this
    // milestone. Demonstrating that the NotSupported path is reachable
    // and typed is the M0b scope-discipline gate.
    match ClockId::try_from_u32(19) {
        Ok(_) => puts("[CPRMAN] UART unexpectedly supported (BUG)\n"),
        Err(ClockError::NotSupported(id)) => {
            puts("[CPRMAN] UART get_rate -> NotSupported (deliberate, not implemented this milestone) id=");
            put_decimal(id as u64);
            puts("\n");
        }
        Err(_) => puts("[CPRMAN] UART unexpected error\n"),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("cprman: starting\n");

    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("cprman: create reply FAILED\n"); halt(); }
    };

    puts("cprman: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("cprman: bootstrap FAILED\n"); halt(); }
    };
    // Reply layout: [server_ep, devmgr_client, _, _]. server_ep is
    // the endpoint we receive clock-op IPCs on; the only legitimate
    // caller is device-manager (the proxy / arbiter — see
    // docs/book-of-lockjaw/03-non-virtualizable-hardware.md).
    let server_ep = EndpointHandle(reply[0]);
    let devmgr_client = EndpointHandle(reply[1]);
    puts("cprman: bootstrapped\n");

    // Claim the CPRMAN device. On QEMU virt this fails (no
    // brcm,bcm2711-cprman); we keep the process alive serving
    // NotSupported for everything so the broker IPC path is still
    // exercised end-to-end on QEMU. On Pi 4B the claim returns the
    // MMIO PageSet handle.
    let claim = match sys_call_ret4(
        devmgr_client, reply_obj, CMD_CLAIM_DEVICE, BCM2711_CPRMAN_HASH, 0, 0,
    ) {
        Ok(r) => r,
        Err(_) => { puts("cprman: claim call FAILED\n"); halt(); }
    };
    let mmio_va = if claim[0] == CLAIM_OK {
        let mmio_pageset = PageSetHandle(claim[1]);
        // Map the first page of the CPRMAN register region. The DTB
        // declares the full region as 0x2000 (8 KB / 2 pages), but
        // the device-manager claim path returns a single-page
        // PageSet today (`sys_register_device_page` in
        // src/cap/pageset_table.rs is explicitly one-page). Both
        // M0b registers we touch (CM_EMMC2CTL = 0x1d0,
        // CM_EMMC2DIV = 0x1d4) are inside the first 4 KB, so 1 page
        // is sufficient. When a future clock leaf needs registers
        // in the second page, the claim-multi-page path will need
        // to land first.
        let va = match VMEM.alloc(1) {
            Some(va) => va,
            None => { puts("cprman: VA exhausted for MMIO\n"); halt(); }
        };
        if !sys_map_pages(mmio_pageset, va, MapMemoryAttribute::Device).is_ok() {
            puts("cprman: map MMIO FAILED\n");
            halt();
        }
        // Run the self-test (prints the three M0b success lines).
        unsafe { self_test(va); }
        Some(va)
    } else {
        puts("[CPRMAN] no BCM2711 CPRMAN on this platform (QEMU); serving NotSupported for all clock ops\n");
        None
    };

    serve(server_ep, mmio_va);
}

/// Server loop. The only legitimate caller is device-manager, which
/// has already validated the binding (caller_token → clock_id) on
/// behalf of the actual driver client. cprman trusts the message
/// body's `clock_id` and dispatches accordingly.
///
/// On platforms where the CPRMAN device wasn't present (QEMU virt),
/// `mmio_va` is `None` and every op replies NotSupported with the
/// requested id echoed back. The IPC plumbing is exercised either
/// way; only the side-effecting MMIO path is conditional.
fn serve(server_ep: EndpointHandle, mmio_va: Option<u64>) -> ! {
    loop {
        let msg = match sys_receive_ret4(server_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let op = msg[0];
        let clock_id_raw = msg[1] as u32;
        let arg = msg[2];

        // Translate the raw id into the typed enum. Unknown ids
        // surface as NotSupported with the offending id echoed —
        // the caller's log message stays meaningful.
        let typed = ClockId::try_from_u32(clock_id_raw);

        // If the device wasn't claimed (QEMU), every leaf is
        // NotSupported regardless of the id. Same wire shape as a
        // real provider that doesn't model this leaf.
        let result = match (mmio_va, typed) {
            (None, _) => Err(ClockError::NotSupported(clock_id_raw)),
            (_, Err(e)) => Err(e),
            (Some(base), Ok(ClockId::Emmc2)) => unsafe { dispatch_emmc2(base, op, arg) },
        };

        // sys_reply returns SyscallError; discard so the match
        // produces the loop-body unit type.
        let _ = match result {
            Ok(value) => sys_reply(CLOCK_OK, value, 0, 0),
            Err(ClockError::NotSupported(id)) =>
                sys_reply(CLOCK_ERR_NOT_SUPPORTED, id as u64, 0, 0),
            Err(e) => sys_reply(e.status_code(), 0, 0, 0),
        };
    }
}

/// Dispatch a clock op to the EMMC2 leaf. SET_RATE / GET_RATE return
/// the actual rate; ENABLE / DISABLE return 0 on success. Unknown
/// opcodes surface as `BadOp`.
unsafe fn dispatch_emmc2(base: u64, op: u64, arg: u64) -> Result<u64, ClockError> {
    match op {
        CLOCK_OP_SET_RATE => emmc2_set_rate(base, arg),
        CLOCK_OP_GET_RATE => emmc2_get_rate(base),
        // ENABLE / DISABLE are no-ops at this milestone:
        // emmc2_set_rate already programs ENABLE as part of the
        // mandatory disable→divider→enable sequence (per
        // bcm2835_clock_on). Standalone gating is M2+ work; for
        // now, accept the op so the IPC contract is complete and
        // return success.
        CLOCK_OP_ENABLE | CLOCK_OP_DISABLE => Ok(0),
        _ => Err(ClockError::BadOp),
    }
}

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("cprman: PANIC\n");
    halt();
}
