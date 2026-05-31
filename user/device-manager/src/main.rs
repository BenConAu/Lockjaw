#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::cell::UnsafeCell;
use lockjaw_userlib::*;
use lockjaw_userlib::syscall::*;
use lockjaw_types::fdt::{parse_fdt_into, FdtDevices};
use lockjaw_types::device::{
    CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR, CMD_RELEASE_BY_ADDR, CLAIM_OK, CLAIM_ERR,
    BCM2711_CPRMAN_HASH, pack_clock_ref,
};
use lockjaw_types::clock::{
    CMD_GET_CLOCK_HANDLE,
    CLOCK_OP_SET_RATE, CLOCK_OP_GET_RATE, CLOCK_OP_ENABLE, CLOCK_OP_DISABLE,
    CLOCK_OK, CLOCK_ERR_INVALID_HANDLE, CLOCK_ERR_TABLE_FULL, CLOCK_ERR_NO_PROVIDER,
};
use lockjaw_types::clock_handle_table::{
    ClockHandleTable, AcquireResult,
};

// FdtDevices is ~18 KB at MAX_DEVICES = 192. Holding it on the stack
// (return-by-value from parse_fdt) would need >36 KB of stack to
// account for the call-site copy. Live in BSS instead so the only
// stack cost is the &mut FdtDevices reference.
//
// SAFETY: device-manager is a single-threaded userspace process; the
// only writer is the bootstrap path before the main IPC loop, the
// only readers are the IPC handler functions running serially after.
struct DevTableCell(UnsafeCell<FdtDevices>);
unsafe impl Sync for DevTableCell {}
static DEVICE_TABLE: DevTableCell = DevTableCell(UnsafeCell::new(FdtDevices::empty()));

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum DTB pages (must match kernel cap in main.rs).
const DTB_MAX_PAGES: usize = 16;

/// device-manager's clock-handle binding table. The data structure
/// + acquire / lookup logic live in
/// `lockjaw_types::clock_handle_table` so the dedup / isolation /
/// exhaustion invariants can be host-tested.
///
/// SAFETY: device-manager is single-threaded; the only writer is the
/// IPC handler functions running serially.
struct ClockTableCell(UnsafeCell<ClockHandleTable>);
unsafe impl Sync for ClockTableCell {}
static CLOCK_TABLE: ClockTableCell =
    ClockTableCell(UnsafeCell::new(ClockHandleTable::empty()));



// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("devmgr: starting\n");

    // Allocate our Reply object for outbound sys_call (just the bootstrap
    // call to init; after that we only reply, we don't call).
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("devmgr: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: call init on handle 0 to receive our server endpoint
    // plus the cprman_client we'll forward CLOCK_OP_* to. The cprman
    // client handle may resolve to a not-yet-spawned cprman process —
    // sys_call on it blocks until cprman is alive and receiving,
    // which is fine because we only call it lazily on first
    // CLOCK_OP_*.
    puts("devmgr: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("devmgr: bootstrap FAILED\n"); halt(); }
    };
    let server_ep = EndpointHandle(reply[0]);
    let cprman_client = EndpointHandle(reply[1]);
    puts("devmgr: bootstrapped, server_ep=");
    put_decimal(reply[0]);
    puts(" cprman_client=");
    put_decimal(reply[1]);
    puts("\n");

    // Step 1: Get the DTB PageSet from the kernel and map it.
    // `sys_get_boot_info` returns both the PageSet handle and the
    // in-page offset of the DTB header within the first page —
    // nonzero on platforms whose firmware places the DTB at an
    // unaligned physical address (Pi 4B's VC firmware typically uses
    // 0xe00). We add the offset to the mapped VA before reading.
    // sys_map_pages with MapMemoryAttribute::Normal maps with normal
    // memory attributes, avoiding the MAIR aliasing problem.
    let boot_info = match sys_get_boot_info() {
        Ok(b) => b,
        Err(_) => { puts("devmgr: get_boot_info FAILED\n"); halt(); }
    };
    let dtb_va = VMEM.alloc(DTB_MAX_PAGES).expect("VA exhausted for DTB");
    if !sys_map_pages(boot_info.dtb_pageset, dtb_va, MapMemoryAttribute::Normal).is_ok() {
        puts("devmgr: DTB map FAILED\n");
        halt();
    }
    let dtb_header_va = dtb_va + boot_info.dtb_in_page_offset as u64;
    puts("devmgr: DTB mapped\n");

    // Step 2: Parse the DTB to discover devices. Read the 40-byte
    // FDT header first to compute the actual content size, then
    // wrap the full content slice. Both reads start at the
    // offset-applied address `dtb_header_va`, not the raw mapping
    // base.
    let dtb_content_end = {
        let header = unsafe { core::slice::from_raw_parts(dtb_header_va as *const u8, 40) };
        match lockjaw_types::fdt::dtb_content_size(header) {
            Ok(size) => size,
            Err(_) => { puts("devmgr: DTB header invalid\n"); halt(); }
        }
    };
    let dtb_slice = unsafe {
        core::slice::from_raw_parts(dtb_header_va as *const u8, dtb_content_end)
    };
    // SAFETY: single-threaded process; this is the only writer to
    // DEVICE_TABLE and runs before any IPC handler can read it.
    let mut devices: &mut FdtDevices = unsafe { &mut *DEVICE_TABLE.0.get() };
    if let Err(_) = parse_fdt_into(dtb_slice, &mut *devices) {
        puts("devmgr: DTB parse FAILED\n");
        halt();
    }
    puts("devmgr: parsed DTB, ");
    put_decimal(devices.count as u64);
    puts(" devices\n");

    // Step 3: Print PL011 device addresses found in the DTB.
    // Reserve the first PL011 for the kernel — scan_platform() takes the
    // first one it finds, so the device manager must match that policy.
    let pl011_hash = PL011_HASH;
    let mut first_pl011 = true;
    for i in 0..devices.count {
        let dev = &devices.devices[i];
        if dev.has_compat(pl011_hash) {
            puts("devmgr: PL011 at ");
            put_hex(dev.mmio_addr);
            puts(" intid=");
            put_decimal(dev.intid as u64);
            if first_pl011 {
                puts(" (kernel, reserved)");
                devices.devices[i].claimed = true;
                first_pl011 = false;
            }
            puts("\n");
        }
    }

    // Step 3: Build the clock-provider registry. For each provider
    // driver we know about (today: cprman), find its DTB phandle so
    // CMD_GET_CLOCK_HANDLE can validate that incoming requests name
    // a real registered controller. On QEMU the cprman device is
    // absent; the phandle stays 0 and every CMD_GET_CLOCK_HANDLE
    // returns NoProvider. See
    // docs/architecture/03-non-virtualizable-hardware.md for the
    // arbitration model.
    let cprman_phandle: u32 = {
        let mut found: u32 = 0;
        for i in 0..devices.count {
            if devices.devices[i].has_compat(BCM2711_CPRMAN_HASH) {
                found = devices.devices[i].phandle;
                break;
            }
        }
        found
    };
    if cprman_phandle != 0 {
        puts("devmgr: registered clock provider cprman, phandle=");
        put_decimal(cprman_phandle as u64);
        puts("\n");
    } else {
        puts("devmgr: no cprman in DTB; clock requests will return NoProvider\n");
    }

    // Step 4: IPC server loop — serve device claim requests.
    puts("devmgr: serving\n");
    loop {
        let msg = match sys_receive_ret4(server_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let cmd = msg[0];

        if cmd == CMD_CLAIM_DEVICE {
            let requested_hash = msg[1];
            let mut found = false;
            for i in 0..devices.count {
                let dev = devices.devices[i];
                if dev.has_compat(requested_hash) && !dev.claimed {
                    // Register the MMIO page as a tracked PageSet
                    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
                        Ok(id) => id,
                        Err(_) => {
                            puts("devmgr: register MMIO page FAILED\n");
                            sys_reply(CLAIM_ERR, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    // Export the MMIO PageSet handle into the claiming
                    // driver's handle table (the caller blocked on our
                    // endpoint). Reply with [status, handle, intid, 0].
                    // Mark claimed AFTER export succeeds — if export fails,
                    // the device stays available for a future claim attempt.
                    let exported = match sys_export_handle(mmio_ps) {
                        Ok(idx) => idx,
                        Err(_) => {
                            sys_close_handle(mmio_ps);
                            puts("devmgr: export MMIO handle FAILED\n");
                            sys_reply(CLAIM_ERR, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    devices.devices[i].claimed = true;
                    devices.devices[i].claim_token = sys_query_caller_token();
                    puts("devmgr: claimed device at ");
                    put_hex(dev.mmio_addr);
                    puts("\n");
                    // Pack the device's first clocks reference into the
                    // claim reply so the driver can immediately call
                    // CMD_GET_CLOCK_HANDLE without a separate query.
                    // 0 means the node had no clocks property.
                    let clock_ref = if dev.clock_count > 0 {
                        pack_clock_ref(
                            dev.clocks[0].controller_phandle,
                            dev.clocks[0].clock_id,
                        )
                    } else {
                        0
                    };
                    sys_reply(CLAIM_OK, exported, dev.intid as u64, clock_ref);
                    found = true;
                    break;
                }
            }
            if !found {
                puts("devmgr: no matching device\n");
                sys_reply(CLAIM_ERR, 0, 0, 0);
            }
        } else if cmd == CMD_PROBE_DEVICE {
            handle_probe_device(&mut devices, &msg);
        } else if cmd == CMD_CLAIM_BY_ADDR {
            handle_claim_by_addr(&mut devices, msg[1]);
        } else if cmd == CMD_RELEASE_BY_ADDR {
            handle_release_by_addr(&mut devices, msg[1]);
        } else if cmd == CMD_GET_CLOCK_HANDLE {
            handle_get_clock_handle(cprman_phandle, &msg);
        } else if cmd == CLOCK_OP_SET_RATE
            || cmd == CLOCK_OP_GET_RATE
            || cmd == CLOCK_OP_ENABLE
            || cmd == CLOCK_OP_DISABLE
        {
            handle_clock_op(reply_obj, cprman_client, cmd, &msg);
        } else {
            sys_reply(0, 0, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Clock-handle binding + op forwarding (proxy through device-manager)
// ---------------------------------------------------------------------------
//
// See docs/architecture/03-non-virtualizable-hardware.md for the
// architectural reasoning. Drivers never receive a direct handle to
// the clock provider; device-manager is the sole arbiter and forwards
// validated ops to cprman with the clock_id encoded in the message
// body.

/// CMD_GET_CLOCK_HANDLE handler — allocate a binding row for the
/// caller and return its opaque handle_id.
///
/// Request:  [CMD_GET_CLOCK_HANDLE, controller_phandle, clock_id, 0]
/// Response: [status, handle_id, 0, 0]
///
/// Validates that `controller_phandle` names a registered provider.
/// Today the only registered provider is cprman (when present in the
/// DTB); any other phandle returns CLOCK_ERR_NO_PROVIDER. See
/// docs/architecture/03-non-virtualizable-hardware.md for why
/// device-manager is the gatekeeper for non-virtualizable hardware.
fn handle_get_clock_handle(cprman_phandle: u32, msg: &[u64; 4]) {
    let controller_phandle = msg[1] as u32;
    let clock_id = msg[2] as u32;
    // The kernel mints nonzero caller tokens (always-mint); callers
    // arriving via bootstrap_endpoint with a literal 0 token would
    // be a kernel-invariant violation, so we surface that as
    // INVALID_HANDLE rather than letting it pass.
    let caller_token = sys_query_caller_token();
    if caller_token == 0 {
        puts("devmgr: clock handle alloc with zero caller token (kernel bug)\n");
        sys_reply(CLOCK_ERR_INVALID_HANDLE, 0, 0, 0);
        return;
    }

    // Validate against the provider registry. cprman_phandle == 0
    // means no provider is registered (e.g., QEMU virt has no
    // bcm2711-cprman); any non-matching phandle when a provider is
    // registered also fails.
    if cprman_phandle == 0 || controller_phandle != cprman_phandle {
        sys_reply(CLOCK_ERR_NO_PROVIDER, 0, 0, 0);
        return;
    }

    // SAFETY: single-threaded; serial IPC dispatch.
    let table = unsafe { &mut *CLOCK_TABLE.0.get() };

    // Idempotent acquire: pure logic in
    // lockjaw_types::clock_handle_table — see host tests there for
    // the dedup / exhaustion / isolation invariants. caller_token is
    // already validated nonzero above, so the unwrap can't fire.
    let token = core::num::NonZeroU64::new(caller_token).unwrap();
    match table.acquire(token, controller_phandle, clock_id) {
        AcquireResult::Existing(id) => {
            sys_reply(CLOCK_OK, id as u64, 0, 0);
        }
        AcquireResult::Allocated(id) => {
            puts("devmgr: clock handle granted (handle_id=");
            put_decimal(id as u64);
            puts(", caller_token=");
            put_decimal(caller_token);
            puts(", controller_phandle=");
            put_decimal(controller_phandle as u64);
            puts(", clock_id=");
            put_decimal(clock_id as u64);
            puts(")\n");
            sys_reply(CLOCK_OK, id as u64, 0, 0);
        }
        AcquireResult::TableFull => {
            puts("devmgr: clock handle table full\n");
            sys_reply(CLOCK_ERR_TABLE_FULL, 0, 0, 0);
        }
    }
}

/// CLOCK_OP_* handler — look up the caller's binding for this
/// handle_id, forward to the provider with clock_id substituted, and
/// relay the reply.
///
/// Request:  [CLOCK_OP_*, handle_id, arg, 0]
/// Forward:  [CLOCK_OP_*, clock_id, arg, 0]
/// Response: relay of provider's reply (untouched).
fn handle_clock_op(
    reply_obj: ReplyHandle,
    cprman_client: EndpointHandle,
    op: u64,
    msg: &[u64; 4],
) {
    let handle_id = msg[1] as u32;
    let arg = msg[2];
    let caller_token = sys_query_caller_token();
    // Token 0 (the master / no-caller sentinel) can never legitimately
    // call us; treat as InvalidHandle. NonZeroU64 makes the lookup
    // signature reject it at the type level rather than relying on a
    // runtime check inside the table.
    let token = match core::num::NonZeroU64::new(caller_token) {
        Some(t) => t,
        None => {
            sys_reply(CLOCK_ERR_INVALID_HANDLE, 0, 0, 0);
            return;
        }
    };

    // SAFETY: single-threaded.
    let table = unsafe { &*CLOCK_TABLE.0.get() };
    let binding = match table.lookup(token, handle_id) {
        Some(b) => b,
        None => {
            sys_reply(CLOCK_ERR_INVALID_HANDLE, 0, 0, 0);
            return;
        }
    };

    // Forward to the provider with clock_id in the message body.
    // Provider trusts the body because device-manager is its only
    // legitimate caller (no driver holds a handle to it).
    //
    // Today there is exactly one provider (cprman); future
    // multi-provider boards would dispatch on
    // binding.controller_phandle here.
    let provider_reply = sys_call_ret4(
        cprman_client,
        reply_obj,
        op,
        binding.clock_id as u64,
        arg,
        0,
    );
    match provider_reply {
        Ok(r) => { sys_reply(r[0], r[1], r[2], r[3]); }
        Err(_) => {
            // Provider unreachable. NO_PROVIDER is the closest typed
            // status; the typical cause is the provider hasn't
            // bootstrapped yet (sys_call would normally block, so
            // this branch fires only on a hard handle error).
            puts("devmgr: clock op forward to provider FAILED\n");
            sys_reply(CLOCK_ERR_NO_PROVIDER, 0, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// CMD_PROBE_DEVICE handler
// ---------------------------------------------------------------------------

/// Probe the Nth device (absolute index) matching a compatible hash.
///
/// Index is over ALL matching devices in the DTB-derived list,
/// including claimed ones. This makes enumeration stable regardless
/// of concurrent claims by other drivers.
///
/// Response: [status, mmio_addr, intid, device_id].
/// Magic validation is done internally; bad magic → PROBE_ERR.
fn handle_probe_device(devices: &mut lockjaw_types::fdt::FdtDevices, msg: &[u64; 4]) {
    use lockjaw_types::device::*;

    let requested_hash = msg[1];
    let index = msg[2] as usize;

    // Find the Nth device matching the hash (regardless of claimed).
    let mut matched = 0;
    let mut target_idx = None;
    for i in 0..devices.count {
        if devices.devices[i].has_compat(requested_hash) {
            if matched == index {
                target_idx = Some(i);
                break;
            }
            matched += 1;
        }
    }

    let idx = match target_idx {
        Some(i) => i,
        None => {
            sys_reply(PROBE_END, 0, 0, 0);
            return;
        }
    };

    let dev = devices.devices[idx];

    if dev.claimed {
        sys_reply(PROBE_CLAIMED, dev.mmio_addr, dev.intid as u64, 0);
        return;
    }

    // Probe is purely structural: return DTB-derived identity only
    // (mmio_addr + intid). No MMIO mapping, no register peek. The
    // earlier shape — map page, read magic at +0, read device_id at +8 —
    // baked virtio-specific knowledge into a supposedly generic handler:
    //   - the magic check rejected non-virtio devices (PL011's DATA
    //     register at +0 is not a magic sentinel), which Phase 5
    //     surfaced; and
    //   - returning `*(addr+8)` as `device_id` is a virtio convention
    //     leaking into the probe protocol, harmless for other families
    //     today only because the only consumer (virtio-blk) happens to
    //     filter on it.
    //
    // Driver-side validation: per-family helpers in lockjaw-userlib
    // (e.g. `virtio::probe_and_claim_virtio_device`) loop probe →
    // claim_typed → validate (magic, DeviceID) → release-if-wrong →
    // try next. This keeps device-manager device-family-agnostic.
    sys_reply(PROBE_OK, dev.mmio_addr, dev.intid as u64, 0);
}

// ---------------------------------------------------------------------------
// CMD_CLAIM_BY_ADDR handler
// ---------------------------------------------------------------------------

/// Claim a device by its exact MMIO physical address (TOCTOU-safe).
/// The driver discovers mmio_addr via CMD_PROBE_DEVICE, then claims
/// by stable identity — no skip_count, no race.
fn handle_claim_by_addr(devices: &mut lockjaw_types::fdt::FdtDevices, mmio_addr: u64) {
    // Find the device by exact MMIO address.
    let mut target_idx = None;
    for i in 0..devices.count {
        if devices.devices[i].mmio_addr == mmio_addr && !devices.devices[i].claimed {
            target_idx = Some(i);
            break;
        }
    }

    let idx = match target_idx {
        Some(i) => i,
        None => {
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };

    let dev = devices.devices[idx];

    // Register the MMIO page as a tracked PageSet.
    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
        Ok(id) => id,
        Err(_) => {
            puts("devmgr: claim-by-addr register FAILED\n");
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };

    // Export the handle into the caller's handle table.
    let exported = match sys_export_handle(mmio_ps) {
        Ok(idx) => idx,
        Err(_) => {
            sys_close_handle(mmio_ps);
            puts("devmgr: claim-by-addr export FAILED\n");
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };
    // sys_export_handle DUPLICATES into the caller's table (refcounts
    // the underlying object); close our local reference so the only
    // surviving handle is the driver's. Otherwise transient claim
    // failures + retries via CMD_RELEASE_BY_ADDR would accumulate
    // one leaked manager-side handle per retry.
    sys_close_handle(mmio_ps);

    devices.devices[idx].claimed = true;
    // Record the caller's IPC token so only this caller can release.
    devices.devices[idx].claim_token = sys_query_caller_token();
    puts("devmgr: claimed device at ");
    put_hex(dev.mmio_addr);
    puts("\n");
    // Same shape as CMD_CLAIM_DEVICE — pack the device's first
    // clocks reference into the reply so the driver can call
    // CMD_GET_CLOCK_HANDLE without a separate query.
    let clock_ref = if dev.clock_count > 0 {
        pack_clock_ref(
            dev.clocks[0].controller_phandle,
            dev.clocks[0].clock_id,
        )
    } else {
        0
    };
    sys_reply(CLAIM_OK, exported, dev.intid as u64, clock_ref);
}

/// Release a previously claimed device so the same `mmio_addr` becomes
/// claimable again. Verifies the caller's IPC token matches the one
/// recorded on claim — otherwise any process that knew an address
/// could steal another driver's claim. Drivers call this when local
/// setup fails AFTER `CMD_CLAIM_BY_ADDR` succeeded (e.g., VA
/// exhaustion during typed claim). The exported MMIO pageset handle
/// is the caller's responsibility to close separately — this RPC
/// only clears the device-manager's `claimed` bit. Replies CLAIM_OK
/// on success; CLAIM_ERR if no matching device was found OR the
/// caller token does not match the claimant.
fn handle_release_by_addr(devices: &mut lockjaw_types::fdt::FdtDevices, mmio_addr: u64) {
    let caller = sys_query_caller_token();
    for i in 0..devices.count {
        if devices.devices[i].mmio_addr == mmio_addr && devices.devices[i].claimed {
            if devices.devices[i].claim_token != caller {
                // Refuse to release someone else's claim.
                puts("devmgr: release rejected (token mismatch) at ");
                put_hex(mmio_addr);
                puts("\n");
                sys_reply(CLAIM_ERR, 0, 0, 0);
                return;
            }
            devices.devices[i].claimed = false;
            devices.devices[i].claim_token = 0;
            puts("devmgr: released device at ");
            put_hex(mmio_addr);
            puts("\n");
            sys_reply(CLAIM_OK, 0, 0, 0);
            return;
        }
    }
    // No matching claimed device.
    sys_reply(CLAIM_ERR, 0, 0, 0);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Terminate the process. EL0 `wfi`-loops keep the thread in
/// `Running` state from the scheduler's POV — they don't block,
/// they spin a tick-period each iteration after the next IRQ wakes
/// the CPU. Use sys_exit so the scheduler removes us from rotation.
fn halt() -> ! {
    sys_exit();
}

// put_decimal / put_hex are imported from lockjaw_userlib (atomic emits).

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("devmgr: PANIC\n");
    halt();
}
