#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::elf::parse_elf;
use lockjaw_userlib::elf_loader::{plan_elf_load, ElfLoadEntry};

/// Number of 4 KB pages allocated for each spawn's `ProcessMapping`
/// array. 16 pages = 2048 mapping slots — matches the kernel's own
/// init-load mapping buffer (`e16b9cb`) and gives ample headroom for
/// any user binary init might spawn. Bump this and the load-plan and
/// temp-VA reservations below grow with it (they're derived).
const INIT_MAP_ARRAY_PAGES: u64 = 16;

/// Plan-buffer capacity for `plan_elf_load`. Equal to the
/// `ProcessMapping` array capacity so a successfully built plan always
/// has somewhere to land its writes.
const INIT_LOAD_PLAN_CAP: usize =
    INIT_MAP_ARRAY_PAGES as usize * PROCESS_MAPPINGS_PER_PAGE;

/// Pages required to hold an `INIT_LOAD_PLAN_CAP`-entry plan buffer.
/// Computed at compile time from the entry size; the buffer is mapped
/// once at init startup (out of stack — at 2048 entries × ~40 B each
/// it would not fit in init's 8-page stack) and reused across spawns.
const INIT_PLAN_BUFFER_PAGES: u64 = {
    let bytes = (INIT_LOAD_PLAN_CAP * core::mem::size_of::<ElfLoadEntry>()) as u64;
    (bytes + PAGE_SIZE - 1) / PAGE_SIZE
};

/// The child process ELF binary, embedded at compile time.
/// Built by: cd user/hello && cargo build --release
static HELLO_ELF: &[u8] = include_bytes!("../../hello/target/aarch64-unknown-none/release/lockjaw-hello");

/// The UART driver ELF binary, embedded at compile time.
/// Built by: cd user/uart-driver && cargo build --release
static UART_ELF: &[u8] = include_bytes!("../../uart-driver/target/aarch64-unknown-none/release/lockjaw-uart-driver");

/// The device manager ELF binary, embedded at compile time.
/// Built by: cd user/device-manager && cargo build --release
static DEVMGR_ELF: &[u8] = include_bytes!("../../device-manager/target/aarch64-unknown-none/release/lockjaw-device-manager");

/// The ramfb display driver ELF binary, embedded at compile time.
/// Built by: cd user/ramfb-driver && cargo build --release
static RAMFB_ELF: &[u8] = include_bytes!("../../ramfb-driver/target/aarch64-unknown-none/release/lockjaw-ramfb-driver");

/// The display test client ELF binary, embedded at compile time.
/// Built by: cd user/display-test && cargo build --release
static DISPLAY_TEST_ELF: &[u8] = include_bytes!("../../display-test/target/aarch64-unknown-none/release/lockjaw-display-test");

/// The virtio-blk driver ELF binary, embedded at compile time.
/// Built by: cd user/virtio-blk-driver && cargo build --release
static BLK_ELF: &[u8] = include_bytes!("../../virtio-blk-driver/target/aarch64-unknown-none/release/lockjaw-virtio-blk-driver");

/// The POSIX personality server ELF binary, embedded at compile time.
/// Built by: cd user/posix-server && cargo build --release
static POSIX_SERVER_ELF: &[u8] = include_bytes!("../../posix-server/target/aarch64-unknown-none/release/lockjaw-posix-server");

/// The FAT32 filesystem server ELF binary, embedded at compile time.
/// Built by: cd user/fat32-server && cargo build --release
static FAT32_ELF: &[u8] = include_bytes!("../../fat32-server/target/aarch64-unknown-none/release/lockjaw-fat32-server");

/// The FAT32 verification client ELF binary, embedded at compile time.
/// Built by: cd user/fat32-test && cargo build --release
static FAT32_TEST_ELF: &[u8] = include_bytes!("../../fat32-test/target/aarch64-unknown-none/release/lockjaw-fat32-test");

/// The CPRMAN clock-controller driver ELF binary, embedded at compile time.
/// Built by: cd user/cprman-driver && cargo build --release
static CPRMAN_ELF: &[u8] = include_bytes!("../../cprman-driver/target/aarch64-unknown-none/release/lockjaw-cprman-driver");

/// The clock-cap proxy verification client, embedded at compile time.
/// Built by: cd user/clock-test && cargo build --release
static CLOCK_TEST_ELF: &[u8] = include_bytes!("../../clock-test/target/aarch64-unknown-none/release/lockjaw-clock-test");

/// The BCM2711 emmc2 (SDHCI) driver ELF binary, embedded at compile time.
/// Built by: cd user/emmc2-driver && cargo build --release
static EMMC2_ELF: &[u8] = include_bytes!("../../emmc2-driver/target/aarch64-unknown-none/release/lockjaw-emmc2-driver");

/// The sleep-primitive verification client, embedded at compile time.
/// Built by: cd user/sleep-test && cargo build --release
/// Exercises lockjaw-userlib::time::sleep_for + monotonic_now and
/// prints `[SLEEP-TEST] elapsed within tolerance` (asserted by the
/// QEMU integration tests) — locks down the kernel's deadline scan
/// against regressions.
static SLEEP_TEST_ELF: &[u8] = include_bytes!("../../sleep-test/target/aarch64-unknown-none/release/lockjaw-sleep-test");

/// Regression guard for B1.1 (full v0..v31 + FPCR + FPSR save/restore
/// in context_switch). Loads all 32 V registers with a known pattern,
/// yields to invite the scheduler to dispatch a sibling user process
/// (any of the ~14 NEON-on user binaries), then re-reads and asserts
/// every register survived the round-trip. Prints `[NEON-CANARY] PASS`
/// (asserted by the QEMU integration tests) on success.
/// Built by: cd user/neon-canary && cargo build --release
static NEON_CANARY_ELF: &[u8] = include_bytes!("../../neon-canary/target/aarch64-unknown-none/release/lockjaw-neon-canary");

/// The partition-manager ELF binary, embedded at compile time.
/// Built by: cd user/partition-manager && cargo build --release
/// Sits between the raw block server and fat32-server: reads LBA 0,
/// parses the partition table (MBR or bare FAT32), translates sector
/// addresses, and exposes partition_srv_ep as a standard BlockEngine.
static PARTMGR_ELF: &[u8] = include_bytes!("../../partition-manager/target/aarch64-unknown-none/release/lockjaw-partition-manager");

// ---------------------------------------------------------------------------
// ELF spawn helper
// ---------------------------------------------------------------------------

/// Parse an ELF binary, allocate pages, copy segments, and spawn as a new process.
/// `elf_data` is the raw ELF binary. `name` is used for log messages.
/// `map_array_va` is a user VA where the mapping array will be mapped (must be free).
/// `temp_base_va` is a base VA for temporary segment page mappings (must be free).
/// `plan_buf_va` is a user VA where `INIT_PLAN_BUFFER_PAGES` pages of
/// `ElfLoadEntry` storage are already mapped (allocated once at init
/// startup and reused — a 2048-entry plan does not fit on init's 8-page
/// stack, so the buffer lives in mapped memory).
/// Returns true on success.
fn spawn_elf(
    elf_data: &[u8],
    name: &str,
    map_array_va: u64,
    temp_base_va: u64,
    plan_buf_va: u64,
    scratch_ps: PageSetHandle,
    handle_to_copy: EndpointHandle,
    stack_pages: u64,
) -> bool {
    puts("init: parsing ");
    puts(name);
    puts(" ELF...\n");

    let elf_info = match parse_elf(elf_data) {
        Ok(info) => info,
        Err(_) => {
            puts("init: ELF parse FAILED\n");
            return false;
        }
    };

    // Verify child ELF build hash matches ours
    match lockjaw_userlib::elf::find_section_u64(elf_data, ".lockjaw_hash") {
        Some(child_hash) if child_hash == LOCKJAW_SOURCE_HASH => {}
        Some(_) => {
            puts("init: BUILD HASH MISMATCH for ");
            puts(name);
            puts("\n");
            return false;
        }
        None => {}
    }

    // Allocate `INIT_MAP_ARRAY_PAGES` pages for the mapping array. The
    // plan-buffer cap below is derived from the same constant, so the
    // array always has room for every entry the plan produces.
    let map_array_ps = match sys_alloc_pages(INIT_MAP_ARRAY_PAGES) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc map array FAILED\n"); return false; }
    };
    if !sys_map_pages(map_array_ps, map_array_va, MapMemoryAttribute::Normal).is_ok() {
        puts("init: map array FAILED\n");
        return false;
    }
    let map_array = map_array_va as *mut ProcessMapping;

    // Plan capacity == array capacity (see constants at the top of
    // this file). plan_elf_load returns TooManyEntries cleanly if a
    // binary needs more — bumping the cap means raising both
    // constants together. The plan buffer is in mapped memory
    // (allocated once at init startup) rather than on the stack
    // because at INIT_LOAD_PLAN_CAP entries it's far too large for
    // init's 8-page stack.
    //
    // SAFETY: caller of spawn_elf passes a `plan_buf_va` mapped to
    // exactly INIT_PLAN_BUFFER_PAGES of memory, which is sized to hold
    // INIT_LOAD_PLAN_CAP `ElfLoadEntry` structs. The slice lifetime is
    // bounded by this function's stack frame.
    let plan_buf = unsafe {
        core::slice::from_raw_parts_mut(
            plan_buf_va as *mut ElfLoadEntry,
            INIT_LOAD_PLAN_CAP,
        )
    };
    let plan = match plan_elf_load(&elf_info, elf_data.len(), plan_buf) {
        Ok(p) => p,
        Err(_) => { puts("init: elf load plan FAILED\n"); return false; }
    };

    // Allocate ONE PageSet for ALL of the child's segment pages. Each
    // ProcessMapping then references this PageSet at a different
    // page_index. This consumes a single header slot in
    // ProcessTransferPlan rather than one per page, which matters
    // because the plan's MAX_CONSUMED_HEADERS cap is 32 — well below
    // the page count of larger binaries (e.g. posix-server with the
    // embedded musl hello binary spans ~42 pages and would exhaust
    // the cap if each page was its own PageSet).
    let page_count = plan.page_count();
    let segs_ps = match sys_alloc_pages(page_count as u64) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc segments PageSet FAILED\n"); return false; }
    };
    if !sys_map_pages(segs_ps, temp_base_va, MapMemoryAttribute::Normal).is_ok() {
        puts("init: map segments PageSet FAILED\n");
        return false;
    }

    // Zero the entire mapped range up front. sys_alloc_pages doesn't
    // guarantee zeroed pages, and we may leave gaps inside pages
    // (in_page_offset != 0, file_size < PAGE_SIZE) that would
    // otherwise leak whatever stale data was there.
    for i in 0..page_count {
        unsafe { zero_page_at_va(temp_base_va + (i as u64) * PAGE_SIZE); }
    }

    // Apply each entry: copy its file slice at the correct in-page
    // offset and register a ProcessMapping at the entry's page_va,
    // using the same segs_ps PageSet with the entry index as page_index.
    for (i, entry) in plan.entries().iter().enumerate() {
        let temp_va: u64 = temp_base_va + (i as u64) * PAGE_SIZE;
        let (src_start, src_end) = entry.src_file_range;
        if src_end > src_start {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    elf_data[src_start..src_end].as_ptr(),
                    (temp_va + entry.in_page_offset as u64) as *mut u8,
                    src_end - src_start,
                );
            }
        }

        unsafe {
            core::ptr::write(map_array.add(i), ProcessMapping {
                virt_addr: entry.page_va,
                pageset_id: segs_ps.0,
                page_index: i as u64,
                flags: if entry.executable { FLAG_EXECUTABLE } else { 0 },
            });
        }
    }
    let mapping_count = page_count;

    let stack_ps = match sys_alloc_pages(stack_pages) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc stack FAILED\n"); return false; }
    };

    // Build a 16-byte NUL-padded name for the kernel TCB
    let mut name_buf = [0u8; 16];
    let name_bytes = name.as_bytes();
    let copy_len = if name_bytes.len() < 15 { name_bytes.len() } else { 15 };
    name_buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    puts("init: spawning ");
    puts(name);
    puts("...\n");

    let result = sys_create_process(
        map_array_va,
        mapping_count as u64,
        elf_info.entry_point,
        stack_ps,
        scratch_ps,
        handle_to_copy.raw(),
        name_buf.as_ptr() as u64,
    );

    if result.is_ok() {
        puts("init: ");
        puts(name);
        puts(" spawned OK\n");
        true
    } else {
        // Surface the SyscallError code so we can tell *why* the
        // spawn failed without re-instrumenting on each Pi run. The
        // codes match SyscallError constants in
        // lockjaw-types/src/syscall.rs (1=INVALID_HANDLE,
        // 3=OUT_OF_MEMORY, 4=INVALID_PARAMETER, 7=QUEUE_FULL,
        // 12=HANDLE_TABLE_FULL, etc.).
        puts("init: ");
        puts(name);
        puts(" spawn FAILED, sys_create_process err=");
        put_decimal(result.0);
        puts(" (");
        puts(syscall_err_name(result));
        puts(")\n");
        false
    }
}

/// Map a SyscallError to a short human-readable label so the boot
/// log doesn't require cross-referencing constants. Reads as a
/// single line in the failure message.
fn syscall_err_name(e: SyscallError) -> &'static str {
    match e {
        SyscallError::OK                  => "OK",
        SyscallError::INVALID_HANDLE      => "INVALID_HANDLE",
        SyscallError::INSUFFICIENT_RIGHTS => "INSUFFICIENT_RIGHTS",
        SyscallError::OUT_OF_MEMORY       => "OUT_OF_MEMORY",
        SyscallError::INVALID_PARAMETER   => "INVALID_PARAMETER",
        SyscallError::ENDPOINT_BUSY       => "ENDPOINT_BUSY",
        SyscallError::NO_CALLER           => "NO_CALLER",
        SyscallError::QUEUE_FULL          => "QUEUE_FULL",
        SyscallError::NOT_MONOTONIC       => "NOT_MONOTONIC",
        SyscallError::ALREADY_WAITING     => "ALREADY_WAITING",
        SyscallError::WOULD_BLOCK         => "WOULD_BLOCK",
        SyscallError::REPLY_BOUND         => "REPLY_BOUND",
        SyscallError::HANDLE_TABLE_FULL   => "HANDLE_TABLE_FULL",
        SyscallError::UNKNOWN             => "UNKNOWN",
        _                                 => "?",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn alloc_endpoint(label: &str) -> EndpointHandle {
    let ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc "); puts(label); puts(" FAILED\n"); park_forever(); }
    };
    match sys_create_endpoint(ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create "); puts(label); puts(" FAILED\n"); park_forever(); }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from userspace init!\n");

    // EL0 monotonic-time probe. ARMv8 lets EL0 read CNTVCT_EL0 +
    // CNTFRQ_EL0 with `mrs` once CNTKCTL_EL1.EL0VCTEN/EL0PCTEN are
    // set in the kernel timer init. If the trap were still in
    // place these `mrs` instructions would synchronously fault and
    // boot would die here. Reading and printing both proves the
    // gate is open — the substrate the upcoming sleep / deadline
    // primitive depends on.
    let cntfrq: u64;
    let cntvct: u64;
    unsafe {
        asm!(
            "mrs {f}, CNTFRQ_EL0",   // counter frequency in Hz (constant per boot)
            "mrs {v}, CNTVCT_EL0",   // monotonic counter (ticks since boot, virtualised)
            f = out(reg) cntfrq,
            v = out(reg) cntvct,
        );
    }
    puts("init: EL0 CNTFRQ=");
    put_decimal(cntfrq);
    puts(" CNTVCT=");
    put_decimal(cntvct);
    puts("\n");

    // Test sys_alloc_pages
    match sys_alloc_pages(1) {
        Ok(test_ps) => {
            puts("init: alloc_pages(1) OK, id=");
            sys_debug_puts(&[b'0' + test_ps.0 as u8, b'\n']);

            // Test sys_map_pages
            let test_va = VMEM.alloc(1).expect("VA exhausted for test page");
            if sys_map_pages(test_ps, test_va, MapMemoryAttribute::Normal).is_ok() {
                puts("init: map_pages OK\n");
                unsafe {
                    let ptr = test_va as *mut u64;
                    core::ptr::write_volatile(ptr, 0xDEAD_CAFE);
                    let readback = core::ptr::read_volatile(ptr);
                    if readback == 0xDEAD_CAFE {
                        puts("init: mapped memory read/write OK\n");
                    } else {
                        puts("init: mapped memory MISMATCH\n");
                    }
                }
            } else {
                puts("init: map_pages FAILED\n");
            }
        }
        Err(_) => {
            puts("init: alloc_pages FAILED\n");
        }
    }

    // Test sys_export_handle — verify it reaches the kernel and validates correctly.
    // Create an endpoint and try to export a handle on it (no caller blocked → should fail).
    let test_ep_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc FAILED\n"); park_forever(); }
    };
    let test_ep = match sys_create_endpoint(test_ep_ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create endpoint FAILED\n"); park_forever(); }
    };
    match sys_export_handle(test_ep) {
        Err(e) if e == SyscallError::NO_CALLER => {
            puts("init: sys_export_handle validation OK (no caller)\n");
        }
        _ => {
            puts("init: sys_export_handle UNEXPECTED result\n");
        }
    }

    // Test sys_get_boot_info — map the DTB PageSet and verify the
    // magic. The DTB header may not start at the first byte of the
    // mapping if the firmware placed the DTB at an unaligned
    // physical address (Pi 4B's VC firmware typically uses 0xe00 in
    // the low 12 bits); apply `dtb_in_page_offset` from the kernel
    // before reading.
    let boot_info = match sys_get_boot_info() {
        Ok(b) => b,
        Err(_) => { puts("init: get_boot_info FAILED\n"); park_forever(); }
    };
    let dtb_va = VMEM.alloc(16).expect("VA exhausted for DTB"); // 16 pages max
    if sys_map_pages(boot_info.dtb_pageset, dtb_va, MapMemoryAttribute::Normal).is_ok() {
        let dtb_header_va = dtb_va + boot_info.dtb_in_page_offset as u64;
        let magic = unsafe {
            let p = dtb_header_va as *const u8;
            u32::from_be_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
        };
        if magic == 0xd00dfeed {
            puts("init: DTB PageSet OK, magic valid\n");
        } else {
            puts("init: DTB PageSet BAD magic\n");
        }
    } else {
        puts("init: DTB map FAILED\n");
    }

    // Allocate a scratch page for create_process — reused across spawns.
    // The kernel uses it as a temporary Mapping buffer during address space creation.
    let scratch_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc scratch page FAILED\n"); park_forever(); }
    };

    // Create endpoints for IPC infrastructure.
    // ep_handle: UART server endpoint (init sends characters, UART driver serves)
    // devmgr_ep: device-manager server endpoint (drivers send claims, devmgr serves)
    // hello_boot_ep, devmgr_boot_ep, uart_boot_ep: bootstrap endpoints (used once each)
    let ep_handle = alloc_endpoint("uart srv");
    let devmgr_ep = alloc_endpoint("devmgr srv");
    let display_ep = alloc_endpoint("display srv");
    // CPRMAN clock-provider endpoint. The only legitimate caller is
    // device-manager (the arbiter for non-virtualizable hardware —
    // see docs/book-of-lockjaw/03-non-virtualizable-hardware.md);
    // drivers never receive a handle to this.
    let cprman_srv_ep = alloc_endpoint("cprman srv");
    let hello_boot_ep = alloc_endpoint("hello boot");
    let devmgr_boot_ep = alloc_endpoint("devmgr boot");
    let uart_boot_ep = alloc_endpoint("uart boot");
    let ramfb_boot_ep = alloc_endpoint("ramfb boot");
    let display_test_boot_ep = alloc_endpoint("dtest boot");
    let blk_srv_ep = alloc_endpoint("blk srv");
    let blk_boot_ep = alloc_endpoint("blk boot");
    let fat32_srv_ep = alloc_endpoint("fat32 srv");
    let fat32_boot_ep = alloc_endpoint("fat32 boot");
    let fat32_test_boot_ep = alloc_endpoint("fat32-test boot");
    let posix_boot_ep = alloc_endpoint("posix boot");
    let cprman_boot_ep = alloc_endpoint("cprman boot");
    let clock_test_boot_ep = alloc_endpoint("clock-test boot");
    let emmc2_boot_ep = alloc_endpoint("emmc2 boot");
    // M7: emmc2 is a second block backend (alongside virtio-blk on QEMU).
    // Init owns the server endpoint so it can route clients to either
    // backend; emmc2 receives the endpoint via bootstrap reply and
    // calls run_block_server on it.
    let emmc2_blk_srv_ep = alloc_endpoint("emmc2 blk srv");
    let sleep_test_boot_ep = alloc_endpoint("sleep-test boot");
    let neon_canary_boot_ep = alloc_endpoint("neon-canary boot");
    let partmgr_boot_ep = alloc_endpoint("partmgr boot");
    // init owns partition_srv_ep: given to partition-manager (to serve on)
    // and to fat32-server (as its upstream block device). Partition-manager
    // translates sector addresses from fat32's view of LBA 0 to the actual
    // partition start on the physical device.
    let partition_srv_ep = alloc_endpoint("partition srv");

    // Spawn child processes.
    // Allocate VAs for ELF loading. These are reused across spawns
    // since spawn_elf completes before returning.
    //   - INIT_MAP_ARRAY_PAGES for the ProcessMapping array
    //   - INIT_LOAD_PLAN_CAP pages for temporary per-page segment mappings
    //     (one temp VA per plan entry)
    //   - INIT_PLAN_BUFFER_PAGES for the plan buffer itself, which is
    //     too large for the stack at INIT_LOAD_PLAN_CAP entries.
    let map_array_va = VMEM.alloc(INIT_MAP_ARRAY_PAGES as usize)
        .expect("VA exhausted for map array");
    let temp_base_va = VMEM.alloc(INIT_LOAD_PLAN_CAP)
        .expect("VA exhausted for temp pages");
    let plan_buf_va = VMEM.alloc(INIT_PLAN_BUFFER_PAGES as usize)
        .expect("VA exhausted for plan buffer");

    // Allocate + map the plan buffer once. Pages from sys_alloc_pages
    // are zeroed; plan_elf_load only writes (never reads before write)
    // into the prefix it populates, so no per-spawn re-init is needed.
    let plan_buf_ps = match sys_alloc_pages(INIT_PLAN_BUFFER_PAGES) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc plan buffer FAILED\n"); park_forever(); }
    };
    if !sys_map_pages(plan_buf_ps, plan_buf_va, MapMemoryAttribute::Normal).is_ok() {
        puts("init: map plan buffer FAILED\n");
        park_forever();
    }

    spawn_elf(HELLO_ELF, "hello", map_array_va, temp_base_va, plan_buf_va, scratch_ps, hello_boot_ep, 1);
    spawn_elf(DEVMGR_ELF, "device-manager", map_array_va, temp_base_va, plan_buf_va, scratch_ps, devmgr_boot_ep, 8);
    // cprman-driver early so the clock provider is up before any
    // future driver that depends on a clock cap (M1+ emmc2-driver
    // will). On QEMU the CPRMAN device claim fails gracefully and
    // cprman serves NotSupported for every clock op.
    spawn_elf(CPRMAN_ELF, "cprman-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, cprman_boot_ep, 4);
    spawn_elf(UART_ELF, "uart-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, uart_boot_ep, 4);
    spawn_elf(RAMFB_ELF, "ramfb-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, ramfb_boot_ep, 4);
    spawn_elf(BLK_ELF, "blk-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, blk_boot_ep, 4);
    spawn_elf(FAT32_ELF, "fat32-server", map_array_va, temp_base_va, plan_buf_va, scratch_ps, fat32_boot_ep, 4);
    spawn_elf(FAT32_TEST_ELF, "fat32-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, fat32_test_boot_ep, 4);
    spawn_elf(POSIX_SERVER_ELF, "posix-server", map_array_va, temp_base_va, plan_buf_va, scratch_ps, posix_boot_ep, 8);
    spawn_elf(DISPLAY_TEST_ELF, "display-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, display_test_boot_ep, 1);
    spawn_elf(CLOCK_TEST_ELF, "clock-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, clock_test_boot_ep, 1);
    // emmc2-driver spawns after cprman so the clock provider is alive when the
    // driver calls CMD_GET_CLOCK_HANDLE. On QEMU the claim fails immediately
    // (no bcm2711-emmc2 in the virt DTB) and the driver exits cleanly.
    spawn_elf(EMMC2_ELF, "emmc2-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, emmc2_boot_ep, 2);
    // partition-manager spawns after both block backends so its bootstrap
    // can receive active_blk_srv_ep (chosen after the DTB probe below).
    // Bootstrapped between block-server and fat32 so the kernel IPC ordering
    // guarantee holds: fat32's first get_info() blocks until partmgr receives.
    spawn_elf(PARTMGR_ELF, "partition-manager", map_array_va, temp_base_va, plan_buf_va, scratch_ps, partmgr_boot_ep, 2);
    // sleep-test verifies the kernel's deadline/sleep primitive
    // (sys_wait_any with absolute monotonic deadline). It needs no
    // device handles — bootstrap is a synchronization handshake only.
    spawn_elf(SLEEP_TEST_ELF, "sleep-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, sleep_test_boot_ep, 1);
    // neon-canary verifies B1.1's user-mode NEON save/restore in
    // context_switch. Needs no device handles — bootstrap is a
    // synchronization handshake only. Spawned alongside sleep-test
    // because it depends on the steady-state init-spawned siblings
    // running for preemption pressure.
    spawn_elf(NEON_CANARY_ELF, "neon-canary", map_array_va, temp_base_va, plan_buf_va, scratch_ps, neon_canary_boot_ep, 1);

    // Bootstrap hello: export a test notification into its handle table.
    puts("init: waiting for hello bootstrap...\n");
    match sys_receive(hello_boot_ep) {
        Ok(_) => {
            // Verify caller token: the hello process called us via an exported
            // endpoint handle, so the kernel should have assigned a nonzero token.
            let token = sys_query_caller_token();
            if token != 0 {
                puts("init: caller token OK (nonzero)\n");
            } else {
                puts("init: caller token ZERO — token delivery broken!\n");
            }
        }
        Err(_) => {
            puts("init: hello bootstrap receive FAILED\n");
        }
    }
    let test_notif_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc notif FAILED\n"); park_forever(); }
    };
    let test_notif = match sys_create_notification(test_notif_ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create notif FAILED\n"); park_forever(); }
    };
    let exported_idx = match sys_export_handle(test_notif) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export FAILED\n"); park_forever(); }
    };
    sys_reply(exported_idx, 0, 0, 0);
    puts("[BOOTSTRAP] hello\n");

    // Bootstrap device-manager: export devmgr_ep (its server) plus
    // cprman_srv_ep (its only path to forward clock ops to the
    // clock provider — see
    // docs/book-of-lockjaw/03-non-virtualizable-hardware.md). cprman
    // hasn't bootstrapped yet at this point; that's fine, the
    // sys_call inside devmgr's clock-op forwarding will block until
    // cprman is alive and receiving.
    puts("init: waiting for devmgr bootstrap...\n");
    let _ = sys_receive(devmgr_boot_ep);
    let devmgr_ep_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr_ep FAILED\n"); park_forever(); }
    };
    let devmgr_cprman_idx = match sys_export_handle(cprman_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export cprman_srv_ep to devmgr FAILED\n"); park_forever(); }
    };
    sys_reply(devmgr_ep_idx, devmgr_cprman_idx, 0, 0);
    puts("[BOOTSTRAP] devmgr\n");

    // Bootstrap cprman-driver: export cprman_srv_ep (the endpoint
    // it serves clock ops on) plus devmgr_ep (so it can claim its
    // CPRMAN MMIO device). Same shape as uart-driver bootstrap.
    puts("init: waiting for cprman bootstrap...\n");
    let _ = sys_receive(cprman_boot_ep);
    let cprman_srv_idx = match sys_export_handle(cprman_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export cprman_srv_ep to cprman FAILED\n"); park_forever(); }
    };
    let cprman_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to cprman FAILED\n"); park_forever(); }
    };
    sys_reply(cprman_srv_idx, cprman_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] cprman\n");

    // Bootstrap UART driver: export ep_handle (its IPC server) and devmgr_ep (its client).
    puts("init: waiting for uart bootstrap...\n");
    let _ = sys_receive(uart_boot_ep);
    let uart_ep_idx = match sys_export_handle(ep_handle) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export uart ep FAILED\n"); park_forever(); }
    };
    let uart_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to uart FAILED\n"); park_forever(); }
    };
    sys_reply(uart_ep_idx, uart_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] uart\n");

    // Bootstrap ramfb driver: export devmgr_ep (to claim fw_cfg) and
    // display_ep (to serve DDI clients) into its handle table.
    puts("init: waiting for ramfb bootstrap...\n");
    let _ = sys_receive(ramfb_boot_ep);
    let ramfb_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to ramfb FAILED\n"); park_forever(); }
    };
    let ramfb_display_idx = match sys_export_handle(display_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export display_ep to ramfb FAILED\n"); park_forever(); }
    };
    // Order matches driver_bootstrap's expectation: reply[0] = server
    // endpoint, reply[1] = devmgr endpoint. (Earlier ramfb pre-Phase-6
    // expected the reversed order; Phase 6 conversion to driver_bootstrap
    // unified ramfb with the uart/virtio-blk convention.)
    sys_reply(ramfb_display_idx, ramfb_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] ramfb\n");

    // Bootstrap display-test: export display_ep so it can use the DDI.
    puts("init: waiting for display-test bootstrap...\n");
    let _ = sys_receive(display_test_boot_ep);
    let dtest_display_idx = match sys_export_handle(display_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export display_ep to display-test FAILED\n"); park_forever(); }
    };
    sys_reply(dtest_display_idx, 0, 0, 0);
    puts("[BOOTSTRAP] display-test\n");

    // Bootstrap blk-driver: export blk_srv_ep (to serve block clients)
    // and devmgr_ep (to claim virtio device) into its handle table.
    puts("init: waiting for blk bootstrap...\n");
    let _ = sys_receive(blk_boot_ep);
    let blk_srv_idx = match sys_export_handle(blk_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export blk_srv_ep FAILED\n"); park_forever(); }
    };
    let blk_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to blk FAILED\n"); park_forever(); }
    };
    sys_reply(blk_srv_idx, blk_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] blk\n");

    // Bootstrap emmc2 BEFORE fat32 so the chosen block server is
    // already running its receive loop by the time fat32 issues its
    // first sys_call. Codex flagged this ordering during M7 review:
    // if fat32 bootstraps first and we route it to emmc2, fat32 blocks
    // until emmc2 reaches run_block_server later in this loop.
    puts("init: waiting for emmc2 bootstrap...\n");
    let _ = sys_receive(emmc2_boot_ep);
    let emmc2_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr_ep to emmc2 FAILED\n"); park_forever(); }
    };
    let emmc2_blk_srv_idx_for_emmc2 = match sys_export_handle(emmc2_blk_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export emmc2_blk_srv_ep to emmc2 FAILED\n"); park_forever(); }
    };
    // P9.4c: bootstrap reply order changed to match the framework's
    // `driver_bootstrap` convention (reply[0] = server_ep,
    // reply[1] = devmgr_ep) — the same shape blk-driver / uart use.
    // Pre-P9.4c emmc2 read these as `[devmgr, blk_srv, _, _]` with a
    // raw bootstrap; the new emmc2_entry consumes the framework ctx
    // via standard_driver_init_level and would deadlock if init
    // still sent them swapped (probe IPC would target the unstarted
    // blk server endpoint instead of devmgr).
    sys_reply(emmc2_blk_srv_idx_for_emmc2, emmc2_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] emmc2\n");

    // Pick the block backend for fat32-server. On Pi 4B the
    // bcm2711-emmc2 compatible string appears in the DTB; on QEMU it
    // does not. Probe via `has_compatible_hash` on the DTB pageset
    // init already mapped — allocation-free, no IPC.
    //
    // Note: this detects HARDWARE PRESENCE, not driver liveness. If
    // emmc2-driver later fails its claim or selftest on a real Pi,
    // fat32's first sys_call to it will block forever — acceptable
    // for M7 follow-up scope (codex consult). A liveness handshake
    // is out of scope.
    // Build a slice covering exactly the DTB bytes — not the full
    // 16-page VA window (which may extend past the mapped pageset
    // for smaller DTBs and is UB to materialize as a slice). Read
    // the 40-byte FDT header first, compute `dtb_content_size`, then
    // construct the exact slice. Same pattern device-manager uses.
    //
    // A header-derived `size` is untrusted: a corrupt header could
    // claim a size larger than the actual mapping. Bound `size`
    // against `max_dtb_bytes` (16 pages minus the in-page offset of
    // the DTB inside the first mapped page) before materializing
    // the slice. `dtb_content_size` itself uses checked_add so the
    // value passed in here can never wrap usize.
    let dtb_base = dtb_va + boot_info.dtb_in_page_offset as u64;
    let max_dtb_bytes =
        16 * PAGE_SIZE as usize - boot_info.dtb_in_page_offset as usize;
    let use_emmc2 = unsafe {
        let header = core::slice::from_raw_parts(dtb_base as *const u8, 40);
        match lockjaw_userlib::fdt::dtb_content_size(header) {
            Ok(size) if size <= max_dtb_bytes => {
                let body = core::slice::from_raw_parts(dtb_base as *const u8, size);
                lockjaw_userlib::fdt::has_compatible_hash(
                    body, lockjaw_userlib::BCM2711_EMMC2_HASH,
                )
            }
            _ => false,
        }
    };
    let active_blk_srv_ep = if use_emmc2 {
        puts("init: Pi 4B detected (bcm2711-emmc2 in DTB) — fat32 over emmc2\n");
        emmc2_blk_srv_ep
    } else {
        puts("init: no bcm2711-emmc2 in DTB — fat32 over virtio-blk\n");
        blk_srv_ep
    };

    // Bootstrap partition-manager: give it (partition_srv_ep, active_blk_srv_ep).
    // Ordering: block-server is in run_block_server before partmgr bootstraps,
    // so partmgr's LBA 0 read to active_blk_srv_ep cannot deadlock. fat32-server
    // bootstraps after this block, so by the time fat32 calls partition_srv_ep,
    // partmgr has already entered (or is about to enter) run_block_server. The
    // kernel's IPC blocking handles the race window without a ready-signal.
    puts("init: waiting for partmgr bootstrap...\n");
    let _ = sys_receive(partmgr_boot_ep);
    let partmgr_srv_idx = match sys_export_handle(partition_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export partition_srv_ep to partmgr FAILED\n"); park_forever(); }
    };
    let partmgr_blk_idx = match sys_export_handle(active_blk_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export active_blk_srv_ep to partmgr FAILED\n"); park_forever(); }
    };
    sys_reply(partmgr_srv_idx, partmgr_blk_idx, 0, 0);
    puts("[BOOTSTRAP] partmgr\n");

    // Bootstrap fat32-server: export its own server endpoint plus
    // partition_srv_ep (the partition-manager's block interface). Partition-
    // manager translates fat32's sector addresses to the actual partition
    // start on the physical device — fat32 sees a contiguous disk at LBA 0.
    puts("init: waiting for fat32 bootstrap...\n");
    let _ = sys_receive(fat32_boot_ep);
    let fat32_srv_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fat32_srv_ep FAILED\n"); park_forever(); }
    };
    let fat32_blk_idx = match sys_export_handle(partition_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export partition_srv_ep to fat32 FAILED\n"); park_forever(); }
    };
    sys_reply(fat32_srv_idx, fat32_blk_idx, 0, 0);
    puts("[BOOTSTRAP] fat32\n");

    // Bootstrap fat32-test: export fat32_srv_ep so the verification
    // client can speak the FS protocol against the server.
    puts("init: waiting for fat32-test bootstrap...\n");
    let _ = sys_receive(fat32_test_boot_ep);
    let fat32_test_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fat32_srv_ep to fat32-test FAILED\n"); park_forever(); }
    };
    sys_reply(fat32_test_idx, 0, 0, 0);
    puts("[BOOTSTRAP] fat32-test\n");

    // Bootstrap posix-server: export fs_srv_ep so it can forward
    // Phase 1 syscalls (openat / read / close) to fat32-server.
    puts("init: waiting for posix-server bootstrap...\n");
    let _ = sys_receive(posix_boot_ep);
    let posix_fs_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fs to posix-server FAILED\n"); park_forever(); }
    };
    sys_reply(posix_fs_idx, 0, 0, 0);
    puts("[BOOTSTRAP] posix-server\n");

    // Bootstrap clock-test: export devmgr_ep so the test client can
    // exercise CMD_GET_CLOCK_HANDLE + CLOCK_OP_SET_RATE through the
    // proxy. On QEMU the SET_RATE will return NotSupported (no
    // CPRMAN device); the test asserts the proxy plumbing works
    // end-to-end either way.
    puts("init: waiting for clock-test bootstrap...\n");
    let _ = sys_receive(clock_test_boot_ep);
    let clock_test_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr_ep to clock-test FAILED\n"); park_forever(); }
    };
    sys_reply(clock_test_devmgr_idx, 0, 0, 0);
    puts("[BOOTSTRAP] clock-test\n");

    // Bootstrap sleep-test: no handles to export, just a sync reply
    // so the client knows init has acknowledged its startup. The
    // client then drives sleep_for + monotonic_now under its own steam.
    puts("init: waiting for sleep-test bootstrap...\n");
    let _ = sys_receive(sleep_test_boot_ep);
    sys_reply(0, 0, 0, 0);
    puts("[BOOTSTRAP] sleep-test\n");

    // Bootstrap neon-canary: same shape — no handles flow; the canary
    // drives its own load/yield/check loop afterwards.
    puts("init: waiting for neon-canary bootstrap...\n");
    let _ = sys_receive(neon_canary_boot_ep);
    sys_reply(0, 0, 0, 0);
    puts("[BOOTSTRAP] neon-canary\n");

    // Boot complete. Park indefinitely — init has no further work
    // and the kernel cannot let it exit (boot TCB stack is in the
    // linker image, not the KVM pool). The old heartbeat
    // (`ipc_puts + sys_yield`) kept init Ready every 10ms tick,
    // contending for fair round-robin slots with real work like
    // emmc2's busy-poll transfers. The Reply object that fed the
    // heartbeat was deleted along with it.
    let _ = ep_handle;
    park_forever();
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Cannot sys_exit: init's boot TCB stack lives in the linker
    // image, not the KVM pool, so finish_exit panics the kernel.
    // park_forever leaves the panicked thread Blocked, releasing
    // the CPU to anything else still runnable.
    park_forever();
}
