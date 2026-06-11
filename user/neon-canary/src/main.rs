#![no_std]
#![no_main]

// Regression guard for B1.1 (full v0..v31 + FPCR + FPSR save/restore
// in context_switch).
//
// What this test does: spawn TWO threads in the same process, each
// loading all 32 V registers with a thread+iteration-tagged pattern,
// then sys_yield()ing to give up the CPU. With two ready threads in
// the canary process AND the kernel scheduler's round-robin, every
// yield deterministically dispatches the OTHER canary thread —
// crossing context_switch on every iteration. The thread that gave
// up the CPU rolls in with whatever V-register state its sibling
// just stored into the hardware registers; if context_switch does
// NOT preserve v0..v31 / FPCR / FPSR per B1.1, the giver-up reads
// the sibling's pattern instead of its own and reports a MISMATCH.
//
// Why TWO threads (not one + init-spawned siblings): on QEMU -smp 1,
// the other init-spawned binaries are all blocked on sys_receive by
// the time the canary runs, so a single-threaded canary's sys_yield
// finds no other ready thread and the scheduler picks us back
// without invoking context_switch — vacuously passing whether or
// not B1.1 is correct. Two cooperating threads make every yield
// cross context_switch by construction. On Pi 4B (-smp 4) the
// timer-tick path also reaches B1.1's guarantee from a different
// angle; the canary covers both single-CPU yield-driven and
// multi-CPU preempt-driven failure modes with the same code.
//
// Why load all 32 V regs (not just v8..v15): preemption is
// asynchronous — a sibling thread could be using any subset of
// v0..v31. A canary that only verified the AAPCS64-callee-saved
// subset would pass against a SavedContext that incorrectly
// preserved only v8..v15, which was the original (rejected) design.
// Verifying the full architectural file is what makes this canary a
// meaningful test of B1.1's actual invariant.
//
// Why one inline asm block for load+yield+store: any Rust code (or
// compiler-inserted SIMD spill/reload) between the load and the
// yield, or between the yield and the store, would test "the
// compiler's NEON discipline" instead of "the kernel's NEON
// preservation across context_switch". Keeping the three steps inside
// one asm! block forces the compiler to spill any caller-side live V
// regs ONCE before the block (because the block declares it clobbers
// all 32), and reload after — never inside the load/yield/store
// sequence we're measuring.

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use lockjaw_userlib::{
    bootstrap_endpoint, put_decimal, puts, sys_exit,
    MapMemoryAttribute, VMEM,
};
use lockjaw_userlib::syscall::{
    sys_alloc_pages, sys_call_ret4, sys_create_reply, sys_create_thread,
    sys_map_pages, sys_yield,
};

/// Number of load → yield → check iterations per thread. With two
/// cooperating ready threads each iteration guarantees one
/// context_switch; 64 = 128 total switches across the test, with
/// every iteration's check examining 32 V registers.
const ITERATIONS: usize = 64;

/// Shared-memory layout used by main (thread 0) and child (thread 1)
/// to synchronise completion + report failures. Each thread writes
/// only its own slot — but multi-CPU (Pi -smp 4) requires explicit
/// memory ordering: AArch64 permits two ordinary stores from the
/// same CPU to be observed by a remote CPU in either order without
/// an explicit barrier. ptr::write_volatile / ptr::read_volatile do
/// NOT emit barriers, so the child's `failure_count` store and
/// `done_flag` store could drain to L1 in reversed order, letting
/// main observe `done_flag=1, failure_count=0` and false-PASS even
/// when the loop actually saw mismatches.
///
/// Fix: AtomicU64 with Release on the child's done_flag store and
/// Acquire on main's done_flag load. The Release-Acquire pair gives
/// standard happens-before, so main's subsequent failure_count read
/// sees the child's prior failure_count write. AtomicU64 is
/// #[repr(transparent)] over u64, so the page layout is identical to
/// a plain [u64; 2] array and we keep the pre-zeroed alloc behaviour.
#[repr(C)]
struct SharedState {
    /// Set to 1 by each thread when its iteration loop is finished.
    /// Release-ordered store (child) / Acquire-ordered load (main).
    done_flag: [AtomicU64; 2],
    /// Per-thread MISMATCH count; main reads after observing
    /// done_flag=1 via Acquire. Plain Relaxed accesses suffice
    /// because the Release-Acquire pair sequences them.
    failure_count: [AtomicU64; 2],
}

/// VA at which the SharedState page is mapped in the canary process.
/// `VMEM.alloc()` picks the actual VA at runtime.
static mut SHARED_VA: u64 = 0;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("neon-canary: starting\n");

    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("neon-canary: create reply FAILED\n"); sys_exit(); }
    };

    // Bootstrap handshake — same shape as sleep-test. Init waits on
    // our boot endpoint and replies; no handles flow.
    puts("neon-canary: bootstrapping...\n");
    if sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0).is_err() {
        puts("neon-canary: bootstrap FAILED\n");
        sys_exit();
    }
    puts("neon-canary: bootstrapped\n");

    // --- Allocate shared state page (read+written by both threads) ---
    let shared_va = VMEM.alloc(1).expect("VA exhausted for shared state");
    let shared_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("neon-canary: shared alloc FAILED\n"); sys_exit(); }
    };
    if !sys_map_pages(shared_ps, shared_va, MapMemoryAttribute::Normal).is_ok() {
        puts("neon-canary: shared map FAILED\n"); sys_exit();
    }
    // Page comes pre-zeroed from sys_alloc_pages; SharedState's
    // default-zero layout is the desired initial state (no thread
    // done, zero failures).
    unsafe { SHARED_VA = shared_va; }

    // --- Allocate child thread stack ---
    let child_stack_va = VMEM.alloc(1).expect("VA exhausted for child stack");
    let child_stack_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("neon-canary: child stack alloc FAILED\n"); sys_exit(); }
    };
    if !sys_map_pages(child_stack_ps, child_stack_va, MapMemoryAttribute::Normal).is_ok() {
        puts("neon-canary: child stack map FAILED\n"); sys_exit();
    }
    let child_stack_top = child_stack_va + 4096;

    // NK3: sys_create_thread takes two donated PageSet handles
    // for the kernel stack and TCB. Allocate both as 1-page
    // Buddy-origin PageSets — kernel consumes them on success.
    let kstack_ps = match sys_alloc_pages(1) {
        Ok(h) => h,
        Err(_) => { puts("neon-canary: kstack alloc FAILED\n"); sys_exit(); }
    };
    let tcb_ps = match sys_alloc_pages(1) {
        Ok(h) => h,
        Err(_) => { puts("neon-canary: tcb alloc FAILED\n"); sys_exit(); }
    };

    // --- Spawn child thread (becomes thread_id = 1) ---
    if sys_create_thread(
        child_entry as *const () as u64,
        child_stack_top,
        child_stack_va,
        /* arg = */ 1,
        kstack_ps, tcb_ps,
    ).is_err() {
        puts("neon-canary: spawn child FAILED\n"); sys_exit();
    }

    // --- Main thread runs as thread_id = 0 ---
    run_canary(0);

    // Wait for child to publish its done flag. Bounded spin with
    // yields so the scheduler keeps dispatching the child until it
    // finishes its loop. Acquire load pairs with the child's Release
    // store, so the subsequent failure_count read sees the value the
    // child wrote just before flipping done_flag (no false-PASS on
    // -smp 4 where stores from one CPU can otherwise reach a remote
    // CPU's L1 in either order).
    let mut spins: u32 = 0;
    while unsafe { done_flag(1) }.load(Ordering::Acquire) == 0 {
        sys_yield();
        spins += 1;
        if spins > 100_000 {
            puts("neon-canary: child never signalled done\n");
            sys_exit();
        }
    }

    let failures_t0 = unsafe { failure_count(0) }.load(Ordering::Relaxed);
    let failures_t1 = unsafe { failure_count(1) }.load(Ordering::Relaxed);
    let total_failures = failures_t0 + failures_t1;

    puts("[NEON-CANARY] iterations=");
    put_decimal(ITERATIONS as u64);
    puts(" threads=2 checks_per_thread=");
    put_decimal((ITERATIONS * 32) as u64);
    puts(" failures_t0=");
    put_decimal(failures_t0);
    puts(" failures_t1=");
    put_decimal(failures_t1);
    puts("\n");

    if total_failures == 0 {
        // Single-line PASS that the integration test greps for.
        puts("[NEON-CANARY] PASS\n");
    } else {
        // Single-line FAIL — appears whenever B1.1 is regressed.
        puts("[NEON-CANARY] FAIL\n");
    }

    sys_exit();
}

extern "C" fn child_entry(thread_id: u64) -> ! {
    run_canary(thread_id);
    sys_exit();
}

/// `SharedState::done_flag[i]` as a `&'static AtomicU64`.
///
/// SAFETY: SHARED_VA is set once before any thread reads it. AtomicU64
/// is `#[repr(transparent)]` over a u64 with interior mutability, so
/// taking `&AtomicU64` is sound even with concurrent writes from
/// other threads (the whole point of the type). The page lives for
/// the process lifetime.
unsafe fn done_flag(thread_id: usize) -> &'static AtomicU64 {
    let base = SHARED_VA as *const SharedState;
    &(*base).done_flag[thread_id]
}

/// `SharedState::failure_count[i]` as a `&'static AtomicU64`.
unsafe fn failure_count(thread_id: usize) -> &'static AtomicU64 {
    let base = SHARED_VA as *const SharedState;
    &(*base).failure_count[thread_id]
}

/// Per-thread canary body: ITERATIONS times, load V regs from
/// expected[], yield, store V regs to got[], compare. Writes failures
/// + done flag to its own shared-state slot.
fn run_canary(thread_id: u64) {
    let mut failures: u64 = 0;
    for iter in 0..ITERATIONS {
        let expected: [u128; 32] = build_pattern(thread_id, iter as u64);
        let mut got: [u128; 32] = [0u128; 32];

        unsafe { load_yield_store(&expected, &mut got); }

        for reg in 0..32 {
            if expected[reg] != got[reg] {
                failures += 1;
                puts("[NEON-CANARY] MISMATCH t=");
                put_decimal(thread_id);
                puts(" iter=");
                put_decimal(iter as u64);
                puts(" v");
                put_decimal(reg as u64);
                // put_hex already prepends "0x", no need to repeat.
                puts(" expected_lo=");
                lockjaw_userlib::put_hex(expected[reg] as u64);
                puts(" expected_hi=");
                lockjaw_userlib::put_hex((expected[reg] >> 64) as u64);
                puts(" got_lo=");
                lockjaw_userlib::put_hex(got[reg] as u64);
                puts(" got_hi=");
                lockjaw_userlib::put_hex((got[reg] >> 64) as u64);
                puts("\n");
            }
        }
    }
    unsafe {
        // Write failure count first; the done_flag store is Release-
        // ordered so a remote-CPU reader that Acquires done_flag also
        // sees this prior store. Without the explicit Release the
        // two writes could drain to L1 in either order on -smp 4
        // ARM, letting main read done_flag=1 + failure_count=0 and
        // false-PASS even when this loop saw mismatches.
        failure_count(thread_id as usize).store(failures, Ordering::Relaxed);
        done_flag(thread_id as usize).store(1, Ordering::Release);
    }
}

/// Build a 32-entry pattern keyed on (thread_id, iter) so two threads'
/// patterns never collide and consecutive iterations don't either.
/// Each register gets a distinct value with non-zero bits in BOTH
/// halves of the u128 — a NEON corruption that zeros a register or
/// shifts lanes shows up as a hi-half mismatch even if the lo half
/// happens to alias.
fn build_pattern(thread_id: u64, iter: u64) -> [u128; 32] {
    let mut out = [0u128; 32];
    let mut i = 0;
    while i < 32 {
        let lo = (thread_id << 56) | (iter << 16) | (i as u64) | 0xA55A_0000_0000_0000;
        let hi = !lo;
        out[i] = ((hi as u128) << 64) | (lo as u128);
        i += 1;
    }
    out
}

/// Atomic load → yield → store sequence. Inside one asm! block so no
/// compiler-inserted SIMD code can land between the steps. With two
/// cooperating canary threads ready, the svc here deterministically
/// invokes context_switch — round-robin picks the OTHER canary
/// thread, which immediately overwrites all 32 V registers with its
/// own pattern before yielding back. When we resume from svc, our
/// stored-back got[] must equal the expected[] we loaded if and only
/// if context_switch preserved every V register across the swap.
///
/// # Safety
/// `expected` must point to 32 × 16 = 512 bytes of readable, 16-byte-
/// aligned memory. `got` must point to 32 × 16 = 512 bytes of writable,
/// 16-byte-aligned memory. Both are satisfied here because `[u128; 32]`
/// is naturally 16-byte aligned by the Rust ABI.
#[inline(never)]
unsafe fn load_yield_store(expected: &[u128; 32], got: &mut [u128; 32]) {
    asm!(
        // ----- Load v0..v31 from `expected` (16 paired loads, 32 B each).
        "ldp q0,  q1,  [{e}, #(0  * 32)]",   // Load v0, v1   (expected[0..2])
        "ldp q2,  q3,  [{e}, #(1  * 32)]",   // Load v2, v3   (expected[2..4])
        "ldp q4,  q5,  [{e}, #(2  * 32)]",   // Load v4, v5   (expected[4..6])
        "ldp q6,  q7,  [{e}, #(3  * 32)]",   // Load v6, v7   (expected[6..8])
        "ldp q8,  q9,  [{e}, #(4  * 32)]",   // Load v8, v9   (expected[8..10])
        "ldp q10, q11, [{e}, #(5  * 32)]",   // Load v10, v11 (expected[10..12])
        "ldp q12, q13, [{e}, #(6  * 32)]",   // Load v12, v13 (expected[12..14])
        "ldp q14, q15, [{e}, #(7  * 32)]",   // Load v14, v15 (expected[14..16])
        "ldp q16, q17, [{e}, #(8  * 32)]",   // Load v16, v17 (expected[16..18])
        "ldp q18, q19, [{e}, #(9  * 32)]",   // Load v18, v19 (expected[18..20])
        "ldp q20, q21, [{e}, #(10 * 32)]",   // Load v20, v21 (expected[20..22])
        "ldp q22, q23, [{e}, #(11 * 32)]",   // Load v22, v23 (expected[22..24])
        "ldp q24, q25, [{e}, #(12 * 32)]",   // Load v24, v25 (expected[24..26])
        "ldp q26, q27, [{e}, #(13 * 32)]",   // Load v26, v27 (expected[26..28])
        "ldp q28, q29, [{e}, #(14 * 32)]",   // Load v28, v29 (expected[28..30])
        "ldp q30, q31, [{e}, #(15 * 32)]",   // Load v30, v31 (expected[30..32])
        // ----- sys_yield: kernel scheduler runs, picks the other
        //       cooperating canary thread, that thread overwrites
        //       v0..v31 with its own pattern then yields back to us.
        //       svc #0 with x8=1 (SYS_YIELD — verified against
        //       lockjaw-types/src/syscall.rs:87).
        "mov x8, #1",                        // x8 = SYS_YIELD syscall number
        "svc #0",                            // Trap into kernel; scheduler dispatches sibling thread
        // ----- Store v0..v31 to `got` (mirror of the load).
        "stp q0,  q1,  [{g}, #(0  * 32)]",   // Save v0, v1   → got[0..2]
        "stp q2,  q3,  [{g}, #(1  * 32)]",   // Save v2, v3   → got[2..4]
        "stp q4,  q5,  [{g}, #(2  * 32)]",   // Save v4, v5   → got[4..6]
        "stp q6,  q7,  [{g}, #(3  * 32)]",   // Save v6, v7   → got[6..8]
        "stp q8,  q9,  [{g}, #(4  * 32)]",   // Save v8, v9   → got[8..10]
        "stp q10, q11, [{g}, #(5  * 32)]",   // Save v10, v11 → got[10..12]
        "stp q12, q13, [{g}, #(6  * 32)]",   // Save v12, v13 → got[12..14]
        "stp q14, q15, [{g}, #(7  * 32)]",   // Save v14, v15 → got[14..16]
        "stp q16, q17, [{g}, #(8  * 32)]",   // Save v16, v17 → got[16..18]
        "stp q18, q19, [{g}, #(9  * 32)]",   // Save v18, v19 → got[18..20]
        "stp q20, q21, [{g}, #(10 * 32)]",   // Save v20, v21 → got[20..22]
        "stp q22, q23, [{g}, #(11 * 32)]",   // Save v22, v23 → got[22..24]
        "stp q24, q25, [{g}, #(12 * 32)]",   // Save v24, v25 → got[24..26]
        "stp q26, q27, [{g}, #(13 * 32)]",   // Save v26, v27 → got[26..28]
        "stp q28, q29, [{g}, #(14 * 32)]",   // Save v28, v29 → got[28..30]
        "stp q30, q31, [{g}, #(15 * 32)]",   // Save v30, v31 → got[30..32]
        e = in(reg) expected.as_ptr(),
        g = in(reg) got.as_mut_ptr(),
        // svc clobbers x0..x7 (return registers) per the kernel ABI,
        // plus x8 (we wrote SYS_YIELD into it).
        out("x0") _, out("x1") _, out("x2") _, out("x3") _,
        out("x4") _, out("x5") _, out("x6") _, out("x7") _,
        out("x8") _,
        // We touch every V register. Tell the compiler so it spills
        // any caller-side live SIMD before the block and reloads after.
        out("v0")  _, out("v1")  _, out("v2")  _, out("v3")  _,
        out("v4")  _, out("v5")  _, out("v6")  _, out("v7")  _,
        out("v8")  _, out("v9")  _, out("v10") _, out("v11") _,
        out("v12") _, out("v13") _, out("v14") _, out("v15") _,
        out("v16") _, out("v17") _, out("v18") _, out("v19") _,
        out("v20") _, out("v21") _, out("v22") _, out("v23") _,
        out("v24") _, out("v25") _, out("v26") _, out("v27") _,
        out("v28") _, out("v29") _, out("v30") _, out("v31") _,
    );
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("neon-canary: PANIC\n");
    sys_exit();
}
