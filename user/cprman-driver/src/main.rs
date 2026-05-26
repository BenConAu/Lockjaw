#![no_std]
#![no_main]
// Driver-crate body writes zero `unsafe` blocks AND zero
// `#[allow(unsafe_code)]` attributes. The macro-generated boot
// stub in `lockjaw_userlib::boot_stub!` is the single audited
// location for the boot-entry attributes.
//
// `#![deny]` (not `#![forbid]`) so the macro-emitted per-item
// allows on `#[no_mangle]` and `#[link_section]` are honoured.
// Acceptance grep:
// `grep -rn 'allow(unsafe_code)' user/cprman-driver/src/`
// MUST return nothing — driver source contains zero allows; all
// allows are in the lockjaw-userlib macro body.
#![deny(unsafe_code)]

use lockjaw_userlib::clock::{
    run_clock_server, ClockEngine, ClockError,
    CLOCK_OP_SET_RATE, CLOCK_OP_GET_RATE, CLOCK_OP_ENABLE, CLOCK_OP_DISABLE,
    cprman::{ClockId, compute_divider, PLLD_PER_CORE_HZ},
};
use lockjaw_userlib::driver_runtime::{standard_init_no_irq, ProbeClaimError};
use lockjaw_userlib::{boot_stub, put_decimal, puts, sys_exit};
use lockjaw_mmio::region::MappedRegs;
use lockjaw_regs::cprman::{CmEmmc2Ctl, CmEmmc2CtlSrc, CmEmmc2Div, Cprman};
use lockjaw_types::device::BCM2711_CPRMAN_HASH;

// ---------------------------------------------------------------------------
// EMMC2 leaf operations.
//
// PASSWD wrapping is mechanical in the generated `set_*` / `modify_*`
// accessors — the codegen ORs CM_PASSWORD (0x5A) into bits[31:24] of
// every write, so the driver expresses field intent (`with_kill(true)`,
// `with_src(CmEmmc2CtlSrc::PllDPerCore)`) and gets PASSWD for free.
// Failing to include PASSWD is the BCM2711 CPRMAN's silent-drop bug
// class; the type system now forecloses it.
// ---------------------------------------------------------------------------

/// Bounded spin waiting for BUSY to clear after a CTL write that
/// changes a divider or source. Returns `Hardware` on timeout so the
/// caller can surface a typed error instead of hanging the provider.
fn wait_not_busy(regs: &Cprman) -> Result<(), ClockError> {
    for _ in 0..1_000_000 {
        if !regs.cm_emmc2_ctl().busy() {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(ClockError::Hardware)
}

/// Set the EMMC2 clock to `target_hz` (computed against PLLD_PER_CORE).
/// Disables → reprograms divider → re-enables. Returns the actual rate
/// the hardware will produce (may differ from target due to divider
/// quantization — see `compute_divider`).
fn emmc2_set_rate(regs: &Cprman, target_hz: u64) -> Result<u64, ClockError> {
    let (divider, actual_hz) = compute_divider(PLLD_PER_CORE_HZ, target_hz)?;

    // 1. Kill the output before changing DIV. Per Linux's
    //    bcm2835_clock_off the SRC selection is preserved across the
    //    kill so the parent reference counting stays consistent; we
    //    re-write it explicitly to make the field intent local.
    regs.set_cm_emmc2_ctl(
        CmEmmc2Ctl::default()
            .with_kill(true)
            .with_src(CmEmmc2CtlSrc::PllDPerCore),
    );
    // Wait for the kill to actually stop the generator (BUSY drops).
    wait_not_busy(regs)?;

    // 2. Program the new divider while the clock is stopped. The 24-bit
    //    divider splits as DIVI (bits 23:12) and DIVF (bits 11:0). The
    //    generated `with_divi` / `with_divf` setters mask + shift into
    //    place; PASSWD goes on top via codegen.
    let divi = (divider >> 12) & 0xFFF;
    let divf = divider & 0xFFF;
    regs.set_cm_emmc2_div(CmEmmc2Div::default().with_divi(divi).with_divf(divf));

    // 3. Re-enable: drop KILL, set ENABLE, keep SRC. Linux's
    //    bcm2835_clock_on does NOT wait — the hardware sets BUSY once
    //    the generator runs, which is the opposite transition from
    //    what `wait_not_busy` polls for. The write itself is enough.
    regs.set_cm_emmc2_ctl(
        CmEmmc2Ctl::default()
            .with_enable(true)
            .with_src(CmEmmc2CtlSrc::PllDPerCore),
    );

    Ok(actual_hz)
}

/// Read the current EMMC2 output rate by reconstructing the 24-bit
/// divider from the typed DIVI / DIVF accessors.
fn emmc2_get_rate(regs: &Cprman) -> Result<u64, ClockError> {
    let div = regs.cm_emmc2_div();
    // 24-bit divider = (DIVI << 12) | DIVF. Both accessors already
    // mask + right-shift into their natural u32 range.
    let combined = ((div.divi() as u64) << 12) | div.divf() as u64;
    if combined == 0 {
        return Err(ClockError::OutOfRange);
    }
    Ok((PLLD_PER_CORE_HZ * 4096 + combined / 2) / combined)
}

// ---------------------------------------------------------------------------
// Self-test — prints the three M0b success lines so the integration
// harness can match on them.
// ---------------------------------------------------------------------------

fn self_test(regs: &Cprman) {
    puts("[CPRMAN] init: register region mapped, taking ownership\n");

    match emmc2_set_rate(regs, 200_000_000) {
        Ok(actual) => {
            puts("[CPRMAN] EMMC2 set_rate(200_000_000) -> actual=");
            put_decimal(actual);
            puts(" enabled=1\n");
        }
        Err(_) => puts("[CPRMAN] EMMC2 set_rate FAILED\n"),
    }

    // BCM2711 CM_UART id = 19. Not implemented this milestone; the
    // typed `ClockId::try_from_u32` surfaces unknown ids as
    // `NotSupported(id)` so the log line carries the offending id
    // (the M0b scope-discipline gate).
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
// CPRMAN engine — implements ClockEngine for the cprman wire shape.
//
// On platforms where the CPRMAN device wasn't present (QEMU virt),
// `regs` is `None` and every op replies NotSupported with the
// requested id. The IPC plumbing is still exercised end-to-end so
// device-manager binding bookkeeping stays meaningful.
// ---------------------------------------------------------------------------

struct CprmanEngine {
    regs: Option<MappedRegs<Cprman>>,
}

impl ClockEngine for CprmanEngine {
    fn dispatch(&mut self, op: u64, clock_id_raw: u32, arg: u64) -> Result<u64, ClockError> {
        let regs = match self.regs.as_ref() {
            Some(r) => r.regs(),
            // No CPRMAN on this platform — every op surfaces as
            // NotSupported with the requested id echoed back.
            None => return Err(ClockError::NotSupported(clock_id_raw)),
        };
        match ClockId::try_from_u32(clock_id_raw)? {
            ClockId::Emmc2 => match op {
                CLOCK_OP_SET_RATE => emmc2_set_rate(regs, arg),
                CLOCK_OP_GET_RATE => emmc2_get_rate(regs),
                // ENABLE / DISABLE are no-ops at M0b: `emmc2_set_rate`
                // already programs ENABLE as part of the mandatory
                // disable→divider→enable sequence (per
                // bcm2835_clock_on). Standalone gating is M2+ work;
                // accept the op so the IPC contract is complete.
                CLOCK_OP_ENABLE | CLOCK_OP_DISABLE => Ok(0),
                _ => Err(ClockError::BadOp),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Driver main — invoked by the boot_stub! macro. Uses Tier-A
// composable pieces (driver_bootstrap + probe_by_hash + claim_typed)
// instead of `driver_main!`'s standard_driver_init because cprman
// has no IRQ — the standard helper would call `bind_irq`
// unconditionally. Same escape-valve pattern ramfb-driver pioneered
// in Phase 6.
// ---------------------------------------------------------------------------

fn cprman_entry() -> ! {
    puts("cprman: starting\n");

    // Tier-B no-IRQ helper: bootstrap → server_ep check → probe + claim.
    // cprman's failure policy differs from ramfb's: probe/claim
    // failures degrade to `None` regs and serve `NotSupported` for
    // every clock op (clock-test harness still exercises the IPC
    // plumbing end-to-end on QEMU virt where the DTB has no
    // brcm,bcm2711-cprman entry). Only a true bootstrap failure
    // halts the driver.
    let init = match standard_init_no_irq::<Cprman>("cprman", BCM2711_CPRMAN_HASH) {
        Ok(i) => i,
        Err(_) => { puts("cprman: bootstrap FAILED\n"); sys_exit(); }
    };
    // Two-arm match preserves the diagnostic distinction between
    // "no device on this platform" (QEMU virt has no
    // brcm,bcm2711-cprman) and "device present but claim failed"
    // (Pi-side device-manager state issue). Collapsing both into
    // one Err arm would mislabel a real claim failure as "wrong
    // platform" on hardware where the device actually exists.
    // P9.4a: NoIrqInit::regs's Ok arm is now ClaimedRegs { regs,
    // clock_ref }. cprman IS the clock provider — its own DTB node
    // has no `clocks` property — so we ignore clock_ref and store
    // the typed MappedRegs.
    let regs = match init.regs {
        Ok(c) => { self_test(c.regs.regs()); Some(c.regs) }
        Err(ProbeClaimError::Probe(_)) => {
            puts("[CPRMAN] no BCM2711 CPRMAN on this platform (QEMU); serving NotSupported for all clock ops\n");
            None
        }
        Err(ProbeClaimError::Claim(_)) => {
            puts("[CPRMAN] CPRMAN claim FAILED; serving NotSupported for all clock ops\n");
            None
        }
    };

    let mut engine = CprmanEngine { regs };
    run_clock_server(&mut engine, init.server_ep)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("cprman: PANIC\n");
    sys_exit();
}

// ---------------------------------------------------------------------------
// Driver boot — Tier-A `boot_stub!` only (not `driver_main!`), because
// cprman's shape doesn't fit the standard "claim + bind_irq + return
// ctx" helper (no IRQ). The macro is the single audited site for the
// boot `#[allow(unsafe_code)]` attributes; the driver crate body
// itself is `#![deny(unsafe_code)]` with zero allows.
// ---------------------------------------------------------------------------

boot_stub! {
    hash = LOCKJAW_SOURCE_HASH,
    main = cprman_entry,
}
