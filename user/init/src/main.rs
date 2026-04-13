#![no_std]
#![no_main]

use core::arch::asm;

/// The child process ELF binary, embedded at compile time.
/// Built by: cd user/hello && cargo build --release
static HELLO_ELF: &[u8] = include_bytes!("../../hello/target/aarch64-unknown-none/release/lockjaw-hello");

/// The UART driver ELF binary, embedded at compile time.
/// Built by: cd user/uart-driver && cargo build --release
static UART_ELF: &[u8] = include_bytes!("../../uart-driver/target/aarch64-unknown-none/release/lockjaw-uart-driver");

// ---------------------------------------------------------------------------
// Syscall wrappers
// ---------------------------------------------------------------------------

fn putc(c: u8) {
    unsafe {
        asm!("svc #0", in("x0") c as u64, in("x8") 0u64);
    }
}

fn sys_yield() {
    unsafe {
        asm!("svc #0", in("x8") 1u64);
    }
}

fn sys_alloc_pages(count: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") count, in("x8") 6u64, lateout("x0") result);
    }
    result
}

fn sys_map_pages(pageset_id: u64, virt_addr: u64, flags: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") pageset_id, in("x1") virt_addr, in("x2") flags, in("x8") 7u64, lateout("x0") result);
    }
    result
}

fn sys_create_process(mappings_ptr: u64, mapping_count: u64, entry_point: u64, stack_pageset_id: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") mappings_ptr,
            in("x1") mapping_count,
            in("x2") entry_point,
            in("x3") stack_pageset_id,
            in("x8") 8u64,
            lateout("x0") result,
        );
    }
    result
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

// ---------------------------------------------------------------------------
// Minimal ELF64 parser (userspace)
// ---------------------------------------------------------------------------

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;

#[derive(Clone, Copy)]
struct ElfSegment {
    vaddr: u64,
    file_offset: u64,
    file_size: u64,
    mem_size: u64,
    executable: bool,
}

struct ElfInfo {
    entry_point: u64,
    segments: [ElfSegment; 4],
    segment_count: usize,
}

fn parse_elf(data: &[u8]) -> Option<ElfInfo> {
    if data.len() < 64 || data[0..4] != ELF_MAGIC {
        return None;
    }

    // Read entry point (offset 24, 8 bytes, little-endian)
    let entry = u64::from_le_bytes([
        data[24], data[25], data[26], data[27],
        data[28], data[29], data[30], data[31],
    ]);

    // Program header offset (offset 32, 8 bytes)
    let phoff = u64::from_le_bytes([
        data[32], data[33], data[34], data[35],
        data[36], data[37], data[38], data[39],
    ]) as usize;

    // Program header entry size (offset 54, 2 bytes)
    let phentsize = u16::from_le_bytes([data[54], data[55]]) as usize;

    // Number of program headers (offset 56, 2 bytes)
    let phnum = u16::from_le_bytes([data[56], data[57]]) as usize;

    let mut info = ElfInfo {
        entry_point: entry,
        segments: [ElfSegment {
            vaddr: 0, file_offset: 0, file_size: 0, mem_size: 0, executable: false,
        }; 4],
        segment_count: 0,
    };

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + phentsize > data.len() {
            break;
        }

        let p_type = u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]);
        if p_type != PT_LOAD {
            continue;
        }
        if info.segment_count >= 4 {
            break;
        }

        let p_flags = u32::from_le_bytes([data[off+4], data[off+5], data[off+6], data[off+7]]);
        let p_offset = u64::from_le_bytes([
            data[off+8], data[off+9], data[off+10], data[off+11],
            data[off+12], data[off+13], data[off+14], data[off+15],
        ]);
        let p_vaddr = u64::from_le_bytes([
            data[off+16], data[off+17], data[off+18], data[off+19],
            data[off+20], data[off+21], data[off+22], data[off+23],
        ]);
        let p_filesz = u64::from_le_bytes([
            data[off+32], data[off+33], data[off+34], data[off+35],
            data[off+36], data[off+37], data[off+38], data[off+39],
        ]);
        let p_memsz = u64::from_le_bytes([
            data[off+40], data[off+41], data[off+42], data[off+43],
            data[off+44], data[off+45], data[off+46], data[off+47],
        ]);

        info.segments[info.segment_count] = ElfSegment {
            vaddr: p_vaddr,
            file_offset: p_offset,
            file_size: p_filesz,
            mem_size: p_memsz,
            executable: (p_flags & PF_X) != 0,
        };
        info.segment_count += 1;
    }

    Some(info)
}

// ---------------------------------------------------------------------------
// ProcessMapping — must match kernel's process::ProcessMapping layout
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessMapping {
    virt_addr: u64,
    pageset_id: u64,
    page_index: u64,
    flags: u64,
}

const FLAG_EXECUTABLE: u64 = 1 << 0;
const PAGE_SIZE: u64 = 4096;

// ---------------------------------------------------------------------------
// ELF spawn helper
// ---------------------------------------------------------------------------

/// Parse an ELF binary, allocate pages, copy segments, and spawn as a new process.
/// `elf_data` is the raw ELF binary. `name` is used for log messages.
/// `map_array_va` is a user VA where the mapping array will be mapped (must be free).
/// `temp_base_va` is a base VA for temporary segment page mappings (must be free).
/// Returns true on success.
fn spawn_elf(elf_data: &[u8], name: &str, map_array_va: u64, temp_base_va: u64) -> bool {
    puts("init: parsing ");
    puts(name);
    puts(" ELF...\n");

    let elf_info = match parse_elf(elf_data) {
        Some(info) => info,
        None => {
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

            unsafe {
                core::ptr::write_bytes(temp_va as *mut u8, 0, PAGE_SIZE as usize);
            }

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

    // Spawn child processes.
    // Each gets its own mapping array VA and temp VA region to avoid overlap.
    spawn_elf(HELLO_ELF, "hello", 0x0070_0000, 0x00A0_0000);
    spawn_elf(UART_ELF, "uart-driver", 0x0071_0000, 0x00C0_0000);

    loop {
        puts("init: alive\n");
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
