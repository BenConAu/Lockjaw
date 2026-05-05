#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::elf::parse_elf;
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::constants::USER_STACK_BASE;
use lockjaw_userlib::elf_loader::{plan_elf_load, ElfLoadEntry};
use lockjaw_types::posix::{
    compute_va_layout, dispatch, neg_errno, write_linux_stack, Action, DispatchInputs,
    StackInputs, ENOSYS, STACK_LAYOUT_FIXED_BYTES,
};

/// Pre-built statically-linked musl hello binary.
/// Built with patched musl (see musl-lockjaw/).
static POSIX_HELLO: &[u8] = include_bytes!("../../posix-hello/hello");

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

// ---------------------------------------------------------------------------
// ELF loading — ported from user/init/src/main.rs:spawn_elf
// ---------------------------------------------------------------------------

/// Apply one [`ElfLoadEntry`] from a load plan: allocate a fresh page,
/// map it temporarily at `temp_va`, zero it, copy the file slice (if
/// any) into the right in-page offset, and append a `ProcessMapping`
/// for the child's address space at `map_array[mapping_idx]`.
///
/// All planning decisions (page count, file ranges, in-page offsets,
/// bounds checks) live in `lockjaw_types::elf_loader::plan_elf_load`.
/// This function is mechanical execution.
fn apply_elf_load_entry(
    entry: &ElfLoadEntry,
    elf_data: &[u8],
    map_array: *mut ProcessMapping,
    mapping_idx: usize,
    temp_va: u64,
) {
    let ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: seg alloc FAILED\n"); halt(); }
    };
    if !sys_map_pages(ps, temp_va, 0).is_ok() {
        puts("posix: seg map FAILED\n");
        halt();
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
        core::ptr::write(map_array.add(mapping_idx), ProcessMapping {
            virt_addr: entry.page_va,
            pageset_id: ps.0,
            page_index: 0,
            flags: if entry.executable { FLAG_EXECUTABLE } else { 0 },
        });
    }
}

/// Load ELF segments into freshly allocated pages. Returns mapping count.
/// `map_array_va` must point to a mapped page for ProcessMapping entries.
/// `temp_base_va` must have enough free VA for all segment pages.
///
/// All structural decisions live in `plan_elf_load`: page count for
/// each segment, file-range slicing, in-page offsets for unaligned
/// vaddrs, BSS-only pages, bounds and overflow checks. This function
/// just allocates a plan buffer, iterates the plan, and applies each
/// entry.
///
/// The plan buffer is sized for the binaries posix-server expects to
/// spawn (Phase 0: a tiny musl static "hello, lockjaw"). 64 entries =
/// ~2.5 KB on the stack — fits comfortably in posix-server's 8-page
/// stack. Larger binaries that overflow this cap surface as a clean
/// `TooManyEntries` error, not a stack overflow.
const POSIX_ELF_LOAD_BUF: usize = 64;

fn load_elf_segments(
    elf_data: &[u8],
    elf_info: &lockjaw_types::elf::ElfInfo,
    map_array_va: u64,
    temp_base_va: u64,
) -> usize {
    let mut planbuf = [ElfLoadEntry::EMPTY; POSIX_ELF_LOAD_BUF];
    let plan = match plan_elf_load(elf_info, elf_data.len(), &mut planbuf) {
        Ok(p) => p,
        Err(_) => { puts("posix: elf load plan FAILED\n"); halt(); }
    };

    let map_array = map_array_va as *mut ProcessMapping;
    for (i, entry) in plan.entries().iter().enumerate() {
        let temp_va = temp_base_va + (i as u64) * PAGE_SIZE;
        apply_elf_load_entry(entry, elf_data, map_array, i, temp_va);
    }
    plan.page_count()
}

// ---------------------------------------------------------------------------
// Stack layout — Linux initial stack for musl _start
// ---------------------------------------------------------------------------

/// Phase 0 AT_RANDOM seed. Fixed; later phases should plumb real entropy.
/// "Lockjaw!POSIX000" — 16 bytes, deterministic so tests that read the
/// child's view of the seed see the same value every run.
const PHASE0_RANDOM_SEED: [u8; 16] = [
    0x4c, 0x6f, 0x63, 0x6b, 0x6a, 0x61, 0x77, 0x21, // "Lockjaw!"
    0x50, 0x4f, 0x53, 0x49, 0x58, 0x30, 0x30, 0x30, // "POSIX000"
];

/// Build the Linux initial stack the musl child reads from SP. The
/// pure layout writer lives in `lockjaw_types::posix::write_linux_stack`
/// (host-tested); this function is just the side-effect glue:
/// `stack_va` is the personality server's temp mapping of the 4-page
/// stack PageSet, and the layout goes in the top page (which the child
/// sees at `USER_STACK_BASE + 3 * PAGE_SIZE`).
fn write_stack_layout(stack_va: u64) {
    let layout_va = stack_va + 3 * PAGE_SIZE;
    let child_layout_va = USER_STACK_BASE + 3 * PAGE_SIZE;

    // SAFETY: stack_va points to a 4-page mapping the personality server
    // owns; the top page is reserved for the initial stack image.
    let buf = unsafe {
        core::slice::from_raw_parts_mut(layout_va as *mut u8, PAGE_SIZE as usize)
    };

    let argv0 = b"hello\0";
    if let Err(_) = write_linux_stack(buf, &StackInputs {
        argv0,
        random_seed: PHASE0_RANDOM_SEED,
        child_layout_va,
        page_size: PAGE_SIZE,
    }) {
        puts("posix: stack layout FAILED\n");
        halt();
    }
    // Sanity check the layout actually fits — surfaces a buffer-size
    // regression here rather than as a child segfault.
    debug_assert!((PAGE_SIZE as usize) >= STACK_LAYOUT_FIXED_BYTES + argv0.len());
}

// ---------------------------------------------------------------------------
// Side effects for dispatch actions
// ---------------------------------------------------------------------------

/// Apply an [`Action::EmitFromShared`]: copy `len` bytes out of the server's
/// mapping of the shared buffer into one atomic sys_debug_puts, then reply.
/// Splitting the read pointer / length / reply value out of the action lets
/// `dispatch` decide policy (which fds are valid, length cap) without
/// pulling syscalls into lockjaw-types.
fn apply_emit_from_shared(server_shared_va: u64, len: u64, then_reply: u64) {
    // SAFETY: server_shared_va is the personality server's mapping of
    // the shared buffer; the child wrote `len` bytes there before the
    // IPC and is blocked waiting on our reply, so the buffer is stable
    // for the duration of this read.
    let data = unsafe {
        core::slice::from_raw_parts(server_shared_va as *const u8, len as usize)
    };
    sys_debug_puts(data);
    sys_reply(then_reply, 0, 0, 0);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("posix-server: starting\n");

    // --- Bootstrap with init (no handles needed, just complete the handshake) ---
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("posix: reply alloc FAILED\n"); halt(); }
    };
    if sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0).is_err() {
        puts("posix: bootstrap call FAILED\n");
        halt();
    }
    puts("[BOOTSTRAP] posix-server\n");

    // --- Parse embedded POSIX binary ---
    let elf_info = match parse_elf(POSIX_HELLO) {
        Ok(info) => info,
        Err(_) => { puts("posix: ELF parse FAILED\n"); halt(); }
    };

    // --- Compute dynamic VAs from ELF layout ---
    // Pure decision (and overflow / stack-overlap checks) live in
    // lockjaw_types::posix::compute_va_layout (host-tested).
    let layout = match compute_va_layout(&elf_info, USER_STACK_BASE) {
        Ok(l) => l,
        Err(_) => {
            puts("posix: ELF layout overlaps stack or overflows\n");
            halt();
        }
    };
    let child_shared_va = layout.child_shared_va;
    let brk_base = layout.brk_base;

    // --- Allocate working pages for ELF loading ---
    let map_array_va = VMEM.alloc(1).expect("VA exhausted");
    let temp_base_va = VMEM.alloc(128).expect("VA exhausted");
    let scratch_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: scratch alloc FAILED\n"); halt(); }
    };

    let map_array_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: map array alloc FAILED\n"); halt(); }
    };
    if !sys_map_pages(map_array_ps, map_array_va, 0).is_ok() {
        puts("posix: map array FAILED\n");
        halt();
    }

    // --- Load ELF segments ---
    let mapping_count = load_elf_segments(
        POSIX_HELLO, &elf_info, map_array_va, temp_base_va,
    );

    // --- Build stack with Linux initial layout ---
    let stack_pages: u64 = 4;
    let stack_ps = match sys_alloc_pages(stack_pages) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: stack alloc FAILED\n"); halt(); }
    };
    // Map stack temporarily to write the argc/argv/auxv layout
    let temp_stack_va = VMEM.alloc(stack_pages as usize).expect("VA exhausted");
    if !sys_map_pages(stack_ps, temp_stack_va, 0).is_ok() {
        puts("posix: stack map FAILED\n");
        halt();
    }
    for p in 0..stack_pages {
        unsafe { zero_page_at_va(temp_stack_va + p * PAGE_SIZE); }
    }
    write_stack_layout(temp_stack_va);

    // --- Create syscall endpoint (child gets this at handle 0) ---
    let syscall_ep_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: ep alloc FAILED\n"); halt(); }
    };
    let syscall_ep = match sys_create_endpoint(syscall_ep_ps) {
        Ok(ep) => ep,
        Err(_) => { puts("posix: ep create FAILED\n"); halt(); }
    };

    // --- Spawn POSIX child ---
    let mut name_buf = [0u8; 16];
    name_buf[..11].copy_from_slice(b"posix-hello");

    puts("posix-server: spawning posix-hello...\n");
    let result = sys_create_process(
        map_array_va,
        mapping_count as u64,
        elf_info.entry_point,
        stack_ps,
        scratch_ps,
        syscall_ep.raw(),
        name_buf.as_ptr() as u64,
    );
    if !result.is_ok() {
        puts("posix-server: spawn FAILED\n");
        halt();
    }
    puts("posix-server: posix-hello spawned OK\n");

    // --- Syscall dispatch loop ---
    let mut server_shared_va: u64 = 0;

    loop {
        let msg = match sys_receive_ret4(syscall_ep) {
            Ok(m) => m,
            Err(_) => { puts("posix: receive FAILED\n"); halt(); }
        };

        // Pure decision lives in lockjaw_types::posix::dispatch (host-tested).
        // This loop is mechanical execution of the returned Action.
        let action = dispatch(&DispatchInputs {
            nr: msg[0],
            arg1: msg[1],
            arg2: msg[2],
            arg3: msg[3],
        });

        match action {
            Action::PosixInit => {
                // First call from child — set up shared buffer.
                // Allocate page, map in our VA space (server_shared_va).
                let shared_ps = match sys_alloc_pages(1) {
                    Ok(ps) => ps,
                    Err(_) => { puts("posix: shared alloc FAILED\n"); halt(); }
                };
                server_shared_va = VMEM.alloc(1).expect("VA exhausted");
                if !sys_map_pages(shared_ps, server_shared_va, 0).is_ok() {
                    puts("posix: shared map FAILED\n");
                    halt();
                }
                // Export the PageSet into the blocked child's handle table.
                // sys_export_handle implicitly targets the caller from the
                // last sys_receive (via current_reply_paddr).
                let child_idx = match sys_export_handle(shared_ps) {
                    Ok(idx) => idx,
                    Err(_) => { puts("posix: export shared FAILED\n"); halt(); }
                };
                // Reply: [child's PageSet handle, child VA, brk base, 0]
                sys_reply(child_idx, child_shared_va, brk_base, 0);
                puts("posix-server: POSIX_INIT OK\n");
            }

            Action::EmitFromShared { fd: _, len, then_reply } => {
                apply_emit_from_shared(server_shared_va, len, then_reply);
            }

            Action::Exit => {
                puts("posix-server: child exit\n");
                break;
            }

            Action::Reply { words } => {
                sys_reply(words[0], words[1], words[2], words[3]);
            }

            Action::Unknown { nr } => {
                puts("posix: unknown nr=");
                put_hex(nr);
                puts(" -> ENOSYS\n");
                sys_reply(neg_errno(ENOSYS), 0, 0, 0);
            }

            // F.1: dispatch decisions added; runtime handlers wired
            // up in F.2. For now, reply -ENOSYS so a misconfigured
            // musl program calling these doesn't hang.
            Action::FileOpen { .. } | Action::FileRead { .. } | Action::FileClose { .. } => {
                sys_reply(neg_errno(ENOSYS), 0, 0, 0);
            }
        }
    }

    puts("posix-server: done\n");
    sys_exit();
}

// put_hex is imported from lockjaw_userlib (atomic emit).

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("posix-server: PANIC\n");
    halt();
}
