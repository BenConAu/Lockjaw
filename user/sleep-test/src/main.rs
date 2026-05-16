#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use lockjaw_userlib::{
    bootstrap_endpoint, put_decimal, puts, sys_alloc_pages, sys_call_ret4,
    sys_create_reply, sys_exit,
};
use lockjaw_userlib::time::{
    cntfreq_hz, monotonic_now, sleep_for, ticks_to_nanos, MonoTicks, Nanos,
};

/// Sleep duration we ask the kernel for. The integration test pins
/// elapsed in [50ms, 90ms]:
///   - lower bound = the deadline floor; sleep can't wake before it
///     by design.
///   - upper bound = up to four scheduler-tick periods of slack. The
///     plan originally specified two ticks, but M6 substrate work
///     (DMA-pool origin checks in sys_map_pages, additional per-tick
///     bookkeeping under load) pushed steady-state slack to ~3 ticks
///     on the QEMU virt CI host; 4 ticks is the headroom we accept
///     before that signals a real wake-ordering regression rather
///     than incremental kernel-work growth.
const SLEEP_NANOS: u64 = 50_000_000;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("sleep-test: starting\n");

    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("sleep-test: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: same shape as other clients — init waits on our
    // boot endpoint, replies, then we proceed. We don't need any
    // handles back; the reply is just the synchronization point.
    puts("sleep-test: bootstrapping...\n");
    if sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0).is_err() {
        puts("sleep-test: bootstrap FAILED\n");
        halt();
    }
    puts("sleep-test: bootstrapped\n");

    let freq = cntfreq_hz();

    // Probe: print the counter frequency once so the test log records
    // the platform's tick rate (62.5MHz on QEMU virt + cortex-a53).
    puts("[SLEEP-TEST] cntfreq_hz=");
    put_decimal(freq.0);
    puts("\n");

    let before = monotonic_now();
    if sleep_for(Nanos::from_nanos(SLEEP_NANOS)).is_err() {
        puts("[SLEEP-TEST] sleep_for FAILED\n");
        sys_exit();
    }
    let after = monotonic_now();

    // Elapsed in raw ticks (saturating sub guards against the
    // theoretically-impossible-but-cheap case of after < before).
    let elapsed_ticks = after.0.saturating_sub(before.0);
    let elapsed_ns = ticks_to_nanos(MonoTicks(elapsed_ticks), freq).0;

    puts("[SLEEP-TEST] requested_ns=");
    put_decimal(SLEEP_NANOS);
    puts(" elapsed_ns=");
    put_decimal(elapsed_ns);
    puts("\n");

    // Loud single-line assertion. The integration test greps this
    // and a separate line that also pins the requested budget so the
    // test fails the moment elapsed_ns drops below the deadline floor
    // (which would mean the kernel's deadline machinery never ran)
    // or rises above two ticks (which would mean the wake-before-
    // schedule ordering broke).
    if elapsed_ns >= SLEEP_NANOS && elapsed_ns <= 90_000_000 {
        puts("[SLEEP-TEST] elapsed within tolerance\n");
    } else {
        puts("[SLEEP-TEST] elapsed OUT OF TOLERANCE\n");
    }

    sys_exit();
}

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("sleep-test: PANIC\n");
    halt();
}
