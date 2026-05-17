#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::clock::{ClockClient, ClockError};

/// Pi 4B EMMC2 clock leaf id (matches BCM2835_CLOCK_EMMC2 in Linux's
/// `include/dt-bindings/clock/bcm2835.h`). On QEMU there is no
/// CPRMAN device, so this resolves to a NotSupported response from
/// the provider — which is the expected M0c result on virt.
const CPRMAN_EMMC2_CLOCK_ID: u32 = 51;

/// Placeholder controller_phandle. M0c doesn't yet route by phandle
/// in device-manager (single-provider build), so any value works
/// here; M1+ will pull the real phandle from the DTB via the
/// device-manager probe path. Picked a non-zero value so the
/// log line is self-evidently a placeholder.
const PLACEHOLDER_CPRMAN_PHANDLE: u32 = 0xC9C9_C9C9;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("clock-test: starting\n");

    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("clock-test: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: receive devmgr_ep so we can drive CMD_GET_CLOCK_HANDLE
    // and CLOCK_OP_* through the proxy.
    puts("clock-test: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("clock-test: bootstrap FAILED\n"); halt(); }
    };
    let devmgr_client = EndpointHandle(reply[0]);
    puts("clock-test: bootstrapped\n");

    // M0c verification: device-manager validates controller_phandle
    // against its provider registry. On QEMU virt the bcm2711-cprman
    // node is absent from the DTB, so the registry has no cprman
    // entry — any CMD_GET_CLOCK_HANDLE returns NoProvider.
    //
    // We use a deliberately bogus placeholder phandle here to make
    // the assertion explicit: even if QEMU one day grew a cprman
    // node, this would still fail the validation, because the
    // placeholder doesn't match the real one. M1's emmc2-driver
    // will get the real phandle from its DeviceInfo.clocks reference
    // and exercise the OK path on Pi 4B.
    match ClockClient::acquire(
        devmgr_client, reply_obj,
        PLACEHOLDER_CPRMAN_PHANDLE, CPRMAN_EMMC2_CLOCK_ID,
    ) {
        Ok(_) => {
            // Should not happen on QEMU. If we ever see this, the
            // validation path is broken — surface a distinct line so
            // it's easy to grep for.
            puts("[CLOCK-TEST] BUG: CMD_GET_CLOCK_HANDLE accepted a bogus controller_phandle\n");
        }
        Err(ClockError::NoProvider) => {
            puts("[CLOCK-TEST] CMD_GET_CLOCK_HANDLE refused unregistered phandle (expected on QEMU)\n");
        }
        Err(e) => {
            puts("[CLOCK-TEST] CMD_GET_CLOCK_HANDLE unexpected error: ");
            put_clock_error(e);
            puts("\n");
        }
    }

    sys_exit();
}

fn put_clock_error(e: ClockError) {
    match e {
        ClockError::NotSupported(id) => { puts("NotSupported("); put_decimal(id as u64); puts(")"); }
        ClockError::OutOfRange       => puts("OutOfRange"),
        ClockError::Hardware         => puts("Hardware"),
        ClockError::BadOp            => puts("BadOp"),
        ClockError::NoProvider       => puts("NoProvider"),
        ClockError::TableFull        => puts("TableFull"),
        ClockError::InvalidHandle    => puts("InvalidHandle"),
        ClockError::IpcFailed        => puts("IpcFailed"),
    }
}

/// Terminate the process. EL0 `wfi`-loops keep the thread in
/// `Running` state from the scheduler's POV — they don't block,
/// they spin a tick-period each iteration after the next IRQ wakes
/// the CPU. Use sys_exit so the scheduler removes us from rotation.
fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("clock-test: PANIC\n");
    halt();
}
