#![no_std]
#![no_main]

use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::elf::parse_elf;

/// The child process ELF binary, embedded at compile time.
/// Built by: cd user/hello && cargo build --release
static HELLO_ELF: &[u8] = include_bytes!("../../hello/target/aarch64-unknown-none/release/lockjaw-hello");

/// The UART driver ELF binary, embedded at compile time.
/// Built by: cd user/uart-driver && cargo build --release
static UART_ELF: &[u8] = include_bytes!("../../uart-driver/target/aarch64-unknown-none/release/lockjaw-uart-driver");

// ---------------------------------------------------------------------------
// ELF spawn helper
// ---------------------------------------------------------------------------

/// Parse an ELF binary, allocate pages, copy segments, and spawn as a new process.
/// `elf_data` is the raw ELF binary. `name` is used for log messages.
/// `map_array_va` is a user VA where the mapping array will be mapped (must be free).
/// `temp_base_va` is a base VA for temporary segment page mappings (must be free).
/// Returns true on success.
fn spawn_elf(elf_data: &[u8], name: &str, map_array_va: u64, temp_base_va: u64, scratch_ps: u64, handle_to_copy: u64) -> bool {
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

    // Allocate a page for the mapping array
    let map_array_ps = sys_alloc_pages(1);
    sys_map_pages(map_array_ps, map_array_va, 0);
    let map_array = map_array_va as *mut ProcessMapping;
    let mut mapping_count: usize = 0;

    // For each ELF segment: allocate pages, map into our space, copy data
    for i in 0..elf_info.segment_count {
        let seg = &elf_info.segments[i];
        let num_pages = ((seg.mem_size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

        for p in 0..num_pages {
            let ps_id = sys_alloc_pages(1);
            if ps_id == u64::MAX {
                puts("init: alloc for segment FAILED\n");
                return false;
            }

            let temp_va: u64 = temp_base_va + (mapping_count as u64) * PAGE_SIZE;
            if sys_map_pages(ps_id, temp_va, 0) != 0 {
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
                    pageset_id: ps_id,
                    page_index: 0,
                    flags: if seg.executable { FLAG_EXECUTABLE } else { 0 },
                });
            }
            mapping_count += 1;
        }
    }

    let stack_ps = sys_alloc_pages(1);
    if stack_ps == u64::MAX {
        puts("init: alloc stack FAILED\n");
        return false;
    }

    puts("init: spawning ");
    puts(name);
    puts("...\n");
    let result = sys_create_process(
        map_array_va,
        mapping_count as u64,
        elf_info.entry_point,
        stack_ps,
        scratch_ps,
        handle_to_copy,
    );

    if result == 0 {
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
// Main
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from userspace init!\n");

    // Test sys_alloc_pages
    let test_ps = sys_alloc_pages(1);
    if test_ps != u64::MAX {
        puts("init: alloc_pages(1) OK, id=");
        putc(b'0' + test_ps as u8);
        putc(b'\n');
    } else {
        puts("init: alloc_pages FAILED\n");
    }

    // Test sys_map_pages
    let map_result = sys_map_pages(test_ps, 0x0060_0000, 0);
    if map_result == 0 {
        puts("init: map_pages OK\n");
        unsafe {
            let ptr = 0x0060_0000 as *mut u64;
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

    // Allocate a scratch page for create_process — reused across spawns.
    // The kernel uses it as a temporary Mapping buffer during address space creation.
    let scratch_ps = sys_alloc_pages(1);
    if scratch_ps == u64::MAX {
        puts("init: alloc scratch page FAILED\n");
        loop { sys_yield(); }
    }

    // Create an endpoint for communicating with the UART server.
    let ep_ps = sys_alloc_pages(1);
    let ep_handle = sys_create_endpoint(ep_ps);
    puts("init: endpoint created, handle=");
    putc(b'0' + ep_handle as u8);
    putc(b'\n');

    // Spawn child processes.
    // Hello gets no handle. UART driver gets the endpoint handle (becomes handle 0 in child).
    spawn_elf(HELLO_ELF, "hello", 0x0070_0000, 0x00A0_0000, scratch_ps, u64::MAX);
    spawn_elf(UART_ELF, "uart-driver", 0x0071_0000, 0x00C0_0000, scratch_ps, ep_handle);

    // From here on, print via IPC to the UART server.
    loop {
        ipc_puts(ep_handle, "init: alive (via IPC)\n");
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
