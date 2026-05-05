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
    if !sys_map_pages(map_array_ps, map_array_va, 0).is_ok() {
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

    // Apply each entry: allocate, temp-map, zero, copy file slice at the
    // correct in-page offset, register a ProcessMapping at page_va.
    for (i, entry) in plan.entries().iter().enumerate() {
        let ps_id = match sys_alloc_pages(1) {
            Ok(id) => id,
            Err(_) => { puts("init: alloc for segment FAILED\n"); return false; }
        };

        let temp_va: u64 = temp_base_va + (i as u64) * PAGE_SIZE;
        if !sys_map_pages(ps_id, temp_va, 0).is_ok() {
            puts("init: map for segment FAILED\n");
            return false;
        }

        unsafe { zero_page_at_va(temp_va); }

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
                pageset_id: ps_id.0,
                page_index: 0,
                flags: if entry.executable { FLAG_EXECUTABLE } else { 0 },
            });
        }
    }
    let mapping_count = plan.page_count();

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
        puts("init: ");
        puts(name);
        puts(" spawn FAILED\n");
        false
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn alloc_endpoint(label: &str) -> EndpointHandle {
    let ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc "); puts(label); puts(" FAILED\n"); loop { sys_yield(); } }
    };
    match sys_create_endpoint(ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create "); puts(label); puts(" FAILED\n"); loop { sys_yield(); } }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from userspace init!\n");

    // Test sys_alloc_pages
    match sys_alloc_pages(1) {
        Ok(test_ps) => {
            puts("init: alloc_pages(1) OK, id=");
            sys_debug_puts(&[b'0' + test_ps.0 as u8, b'\n']);

            // Test sys_map_pages
            let test_va = VMEM.alloc(1).expect("VA exhausted for test page");
            if sys_map_pages(test_ps, test_va, 0).is_ok() {
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
        Err(_) => { puts("init: alloc FAILED\n"); loop { sys_yield(); } }
    };
    let test_ep = match sys_create_endpoint(test_ep_ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create endpoint FAILED\n"); loop { sys_yield(); } }
    };
    match sys_export_handle(test_ep) {
        Err(e) if e == SyscallError::NO_CALLER => {
            puts("init: sys_export_handle validation OK (no caller)\n");
        }
        _ => {
            puts("init: sys_export_handle UNEXPECTED result\n");
        }
    }

    // Test sys_get_boot_info — map the DTB PageSet and verify the magic.
    let dtb_ps = match sys_get_boot_info() {
        Ok(id) => id,
        Err(_) => { puts("init: get_boot_info FAILED\n"); loop { sys_yield(); } }
    };
    let dtb_va = VMEM.alloc(16).expect("VA exhausted for DTB"); // 16 pages max
    if sys_map_pages(dtb_ps, dtb_va, 0).is_ok() {
        let magic = unsafe {
            let p = dtb_va as *const u8;
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
        Err(_) => { puts("init: alloc scratch page FAILED\n"); loop { sys_yield(); } }
    };

    // Create endpoints for IPC infrastructure.
    // ep_handle: UART server endpoint (init sends characters, UART driver serves)
    // devmgr_ep: device-manager server endpoint (drivers send claims, devmgr serves)
    // hello_boot_ep, devmgr_boot_ep, uart_boot_ep: bootstrap endpoints (used once each)
    let ep_handle = alloc_endpoint("uart srv");
    let devmgr_ep = alloc_endpoint("devmgr srv");
    let display_ep = alloc_endpoint("display srv");
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
        Err(_) => { puts("init: alloc plan buffer FAILED\n"); loop { sys_yield(); } }
    };
    if !sys_map_pages(plan_buf_ps, plan_buf_va, 0).is_ok() {
        puts("init: map plan buffer FAILED\n");
        loop { sys_yield(); }
    }

    spawn_elf(HELLO_ELF, "hello", map_array_va, temp_base_va, plan_buf_va, scratch_ps, hello_boot_ep, 1);
    spawn_elf(DEVMGR_ELF, "device-manager", map_array_va, temp_base_va, plan_buf_va, scratch_ps, devmgr_boot_ep, 8);
    spawn_elf(UART_ELF, "uart-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, uart_boot_ep, 4);
    spawn_elf(RAMFB_ELF, "ramfb-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, ramfb_boot_ep, 4);
    spawn_elf(BLK_ELF, "blk-driver", map_array_va, temp_base_va, plan_buf_va, scratch_ps, blk_boot_ep, 4);
    spawn_elf(FAT32_ELF, "fat32-server", map_array_va, temp_base_va, plan_buf_va, scratch_ps, fat32_boot_ep, 4);
    spawn_elf(FAT32_TEST_ELF, "fat32-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, fat32_test_boot_ep, 4);
    spawn_elf(POSIX_SERVER_ELF, "posix-server", map_array_va, temp_base_va, plan_buf_va, scratch_ps, posix_boot_ep, 8);
    spawn_elf(DISPLAY_TEST_ELF, "display-test", map_array_va, temp_base_va, plan_buf_va, scratch_ps, display_test_boot_ep, 1);

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
        Err(_) => { puts("init: alloc notif FAILED\n"); loop { sys_yield(); } }
    };
    let test_notif = match sys_create_notification(test_notif_ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create notif FAILED\n"); loop { sys_yield(); } }
    };
    let exported_idx = match sys_export_handle(test_notif) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(exported_idx, 0, 0, 0);
    puts("[BOOTSTRAP] hello\n");

    // Bootstrap device-manager: export devmgr_ep so it can serve device claims.
    puts("init: waiting for devmgr bootstrap...\n");
    let _ = sys_receive(devmgr_boot_ep);
    let devmgr_ep_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr_ep FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(devmgr_ep_idx, 0, 0, 0);
    puts("[BOOTSTRAP] devmgr\n");

    // Bootstrap UART driver: export ep_handle (its IPC server) and devmgr_ep (its client).
    puts("init: waiting for uart bootstrap...\n");
    let _ = sys_receive(uart_boot_ep);
    let uart_ep_idx = match sys_export_handle(ep_handle) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export uart ep FAILED\n"); loop { sys_yield(); } }
    };
    let uart_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to uart FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(uart_ep_idx, uart_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] uart\n");

    // Bootstrap ramfb driver: export devmgr_ep (to claim fw_cfg) and
    // display_ep (to serve DDI clients) into its handle table.
    puts("init: waiting for ramfb bootstrap...\n");
    let _ = sys_receive(ramfb_boot_ep);
    let ramfb_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to ramfb FAILED\n"); loop { sys_yield(); } }
    };
    let ramfb_display_idx = match sys_export_handle(display_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export display_ep to ramfb FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(ramfb_devmgr_idx, ramfb_display_idx, 0, 0);
    puts("[BOOTSTRAP] ramfb\n");

    // Bootstrap display-test: export display_ep so it can use the DDI.
    puts("init: waiting for display-test bootstrap...\n");
    let _ = sys_receive(display_test_boot_ep);
    let dtest_display_idx = match sys_export_handle(display_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export display_ep to display-test FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(dtest_display_idx, 0, 0, 0);
    puts("[BOOTSTRAP] display-test\n");

    // Bootstrap blk-driver: export blk_srv_ep (to serve block clients)
    // and devmgr_ep (to claim virtio device) into its handle table.
    puts("init: waiting for blk bootstrap...\n");
    let _ = sys_receive(blk_boot_ep);
    let blk_srv_idx = match sys_export_handle(blk_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export blk_srv_ep FAILED\n"); loop { sys_yield(); } }
    };
    let blk_devmgr_idx = match sys_export_handle(devmgr_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export devmgr to blk FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(blk_srv_idx, blk_devmgr_idx, 0, 0);
    puts("[BOOTSTRAP] blk\n");

    // Bootstrap fat32-server: export its own server endpoint (so it
    // can sys_receive on it once Phase E wires up clients) plus the
    // block-driver endpoint (so it can read sectors).
    puts("init: waiting for fat32 bootstrap...\n");
    let _ = sys_receive(fat32_boot_ep);
    let fat32_srv_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fat32_srv_ep FAILED\n"); loop { sys_yield(); } }
    };
    let fat32_blk_idx = match sys_export_handle(blk_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export blk to fat32 FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(fat32_srv_idx, fat32_blk_idx, 0, 0);
    puts("[BOOTSTRAP] fat32\n");

    // Bootstrap fat32-test: export fat32_srv_ep so the verification
    // client can speak the FS protocol against the server.
    puts("init: waiting for fat32-test bootstrap...\n");
    let _ = sys_receive(fat32_test_boot_ep);
    let fat32_test_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fat32_srv_ep to fat32-test FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(fat32_test_idx, 0, 0, 0);
    puts("[BOOTSTRAP] fat32-test\n");

    // Bootstrap posix-server: export fs_srv_ep so it can forward
    // Phase 1 syscalls (openat / read / close) to fat32-server.
    puts("init: waiting for posix-server bootstrap...\n");
    let _ = sys_receive(posix_boot_ep);
    let posix_fs_idx = match sys_export_handle(fat32_srv_ep) {
        Ok(idx) => idx,
        Err(_) => { puts("init: export fs to posix-server FAILED\n"); loop { sys_yield(); } }
    };
    sys_reply(posix_fs_idx, 0, 0, 0);
    puts("[BOOTSTRAP] posix-server\n");

    // Allocate a Reply object for init's own outbound calls (ipc_puts to
    // the uart server). Each client that issues sys_call needs one.
    let init_reply_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc reply page FAILED\n"); loop { sys_yield(); } }
    };
    let init_reply = match sys_create_reply(init_reply_ps) {
        Ok(h) => h,
        Err(_) => { puts("init: create reply FAILED\n"); loop { sys_yield(); } }
    };

    // Print via IPC to the UART server.
    loop {
        ipc_puts(ep_handle, init_reply, "init: alive (via IPC)\n");
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
