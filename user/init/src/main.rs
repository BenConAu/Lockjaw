#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::elf::parse_elf;

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

// ---------------------------------------------------------------------------
// ELF spawn helper
// ---------------------------------------------------------------------------

/// Parse an ELF binary, allocate pages, copy segments, and spawn as a new process.
/// `elf_data` is the raw ELF binary. `name` is used for log messages.
/// `map_array_va` is a user VA where the mapping array will be mapped (must be free).
/// `temp_base_va` is a base VA for temporary segment page mappings (must be free).
/// Returns true on success.
fn spawn_elf(elf_data: &[u8], name: &str, map_array_va: u64, temp_base_va: u64, scratch_ps: PageSetHandle, handle_to_copy: EndpointHandle, stack_pages: u64) -> bool {
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

    // Allocate a page for the mapping array
    let map_array_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("init: alloc map array FAILED\n"); return false; }
    };
    if !sys_map_pages(map_array_ps, map_array_va, 0).is_ok() {
        puts("init: map array FAILED\n");
        return false;
    }
    let map_array = map_array_va as *mut ProcessMapping;
    let mut mapping_count: usize = 0;

    // For each ELF segment: allocate pages, map into our space, copy data
    for i in 0..elf_info.segment_count {
        let seg = &elf_info.segments[i];
        let num_pages = ((seg.mem_size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

        for p in 0..num_pages {
            let ps_id = match sys_alloc_pages(1) {
                Ok(id) => id,
                Err(_) => { puts("init: alloc for segment FAILED\n"); return false; }
            };

            let temp_va: u64 = temp_base_va + (mapping_count as u64) * PAGE_SIZE;
            if !sys_map_pages(ps_id, temp_va, 0).is_ok() {
                puts("init: map for segment FAILED\n");
                return false;
            }

            unsafe { zero_page_at_va(temp_va); }

            let seg_page_offset = (p as u64) * PAGE_SIZE;
            let file_remaining = if seg.file_size > seg_page_offset {
                let r = seg.file_size - seg_page_offset;
                if r > PAGE_SIZE { PAGE_SIZE } else { r }
            } else {
                0
            };

            if file_remaining > 0 {
                let src_start = (seg.file_offset + seg_page_offset) as usize;
                let src_end = src_start + file_remaining as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        elf_data[src_start..src_end].as_ptr(),
                        temp_va as *mut u8,
                        file_remaining as usize,
                    );
                }
            }

            unsafe {
                core::ptr::write(map_array.add(mapping_count), ProcessMapping {
                    virt_addr: seg.vaddr + seg_page_offset,
                    pageset_id: ps_id.0,
                    page_index: 0,
                    flags: if seg.executable { FLAG_EXECUTABLE } else { 0 },
                });
            }
            mapping_count += 1;
        }
    }

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
            putc(b'0' + test_ps.0 as u8);
            putc(b'\n');

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

    // Spawn child processes.
    // Allocate temp VAs for ELF loading. Each spawn needs:
    // - 1 page for the mapping array
    // - N pages for temporary segment mappings (generous: 128 pages = 512KB)
    // These are reused across spawns since spawn_elf completes before returning.
    let map_array_va = VMEM.alloc(1).expect("VA exhausted for map array");
    let temp_base_va = VMEM.alloc(128).expect("VA exhausted for temp pages");

    spawn_elf(HELLO_ELF, "hello", map_array_va, temp_base_va, scratch_ps, hello_boot_ep, 1);
    spawn_elf(DEVMGR_ELF, "device-manager", map_array_va, temp_base_va, scratch_ps, devmgr_boot_ep, 8);
    spawn_elf(UART_ELF, "uart-driver", map_array_va, temp_base_va, scratch_ps, uart_boot_ep, 4);
    spawn_elf(RAMFB_ELF, "ramfb-driver", map_array_va, temp_base_va, scratch_ps, ramfb_boot_ep, 4);

    // Bootstrap hello: export a test notification into its handle table.
    puts("init: waiting for hello bootstrap...\n");
    let _ = sys_receive(hello_boot_ep);
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
