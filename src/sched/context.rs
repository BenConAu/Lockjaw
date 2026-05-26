use core::arch::global_asm;

// SavedContext struct and layout assertions live in lockjaw-types
// (host-testable). Re-export so existing kernel imports work.
pub use lockjaw_types::thread::SavedContext;

extern "C" {
    /// Switch from the current thread to another.
    /// Saves callee-saved registers on the current stack, stores SP,
    /// loads SP from the new thread, restores callee-saved registers, returns.
    ///
    /// # Arguments
    /// * `old_sp_ptr` — pointer to the old thread's saved_sp field in its TCB
    /// * `new_sp_ptr` — pointer to the new thread's saved_sp field in its TCB
    pub fn context_switch(old_sp_ptr: *mut u64, new_sp_ptr: *const u64);
}

global_asm!(
    r#"
// Enable FP + SIMD encoding for this asm block so the assembler
// accepts the q-form stp/ldp and the FPCR/FPSR system-register
// mnemonics used to swap user-mode NEON state across threads.
//
// The kernel build itself targets aarch64-unknown-none-softfloat,
// which forbids rustc from emitting any NEON instruction in
// compiler-generated code (the invariant verified by xtask
// check-kernel-no-neon). This asm block is the one approved
// exception: it doesn't *use* NEON registers as kernel scratch,
// it merely *spills and reloads* the user thread's v0..v31 / FPCR /
// FPSR across a context switch. The arch_extension directive only
// affects what mnemonics the assembler will encode here; it does
// not relax the soft-float target's effect on Rust code anywhere.
.arch_extension fp
.arch_extension simd

.global context_switch
context_switch:
    // x0 = &old_tcb.saved_sp, x1 = &new_tcb.saved_sp.
    //
    // SavedContext layout (sizeof = 624, 16-byte aligned):
    //   offset 0..95   x19..x30        (12 × 8 = 96 B)
    //   offset 96      fpcr            (8 B)
    //   offset 104     fpsr            (8 B)
    //   offset 112..623  v0..v31        (32 × 16 = 512 B)
    // Field choices and constraints: see SavedContext doc in
    // lockjaw_types::thread. 624 = 39 × 16 keeps SP 16-aligned for
    // the q-form stp/ldp; fpcr/fpsr sit at low offsets so the x-form
    // ±504 immediate window encodes them; vregs land last because
    // the q-form's ±1008 window tolerates the larger offsets.

    // --- Allocate SavedContext on current stack ---
    sub     sp, sp, #624                 // Allocate full SavedContext (624 B = 12 GPRs + fpcr/fpsr + 32 vregs)

    // --- Save callee-saved GPRs (offsets 0..95) ---
    stp     x19, x20, [sp, #(0  * 8)]   // Save x19, x20
    stp     x21, x22, [sp, #(2  * 8)]   // Save x21, x22
    stp     x23, x24, [sp, #(4  * 8)]   // Save x23, x24
    stp     x25, x26, [sp, #(6  * 8)]   // Save x25, x26
    stp     x27, x28, [sp, #(8  * 8)]   // Save x27, x28
    stp     x29, x30, [sp, #(10 * 8)]   // Save x29 (FP), x30 (LR)

    // --- Save FPCR + FPSR (offsets 96..111) ---
    // Kernel is soft-float (aarch64-unknown-none-softfloat) so we
    // can scratch x2/x3 here without disturbing live NEON state in
    // the v registers — the preempted user thread's state lives in
    // v0..v31, not in any GPR the kernel will clobber.
    mrs     x2, FPCR                     // x2 = current FPCR (user-mode FP control state)
    mrs     x3, FPSR                     // x3 = current FPSR (user-mode FP sticky flags)
    stp     x2, x3, [sp, #96]            // Save fpcr/fpsr as adjacent 8-byte slots

    // --- Save v0..v31 (offsets 112..623) ---
    // 16 paired stores of 128-bit Q registers. Each pair writes
    // 32 bytes, so consecutive offsets advance by 32. Range check:
    // last offset = 112 + 15*32 = 592, well inside the q-form
    // ±1008-byte signed scaled immediate window.
    stp     q0,  q1,  [sp, #(112 +  0 * 32)]  // Save v0, v1
    stp     q2,  q3,  [sp, #(112 +  1 * 32)]  // Save v2, v3
    stp     q4,  q5,  [sp, #(112 +  2 * 32)]  // Save v4, v5
    stp     q6,  q7,  [sp, #(112 +  3 * 32)]  // Save v6, v7
    stp     q8,  q9,  [sp, #(112 +  4 * 32)]  // Save v8, v9
    stp     q10, q11, [sp, #(112 +  5 * 32)]  // Save v10, v11
    stp     q12, q13, [sp, #(112 +  6 * 32)]  // Save v12, v13
    stp     q14, q15, [sp, #(112 +  7 * 32)]  // Save v14, v15
    stp     q16, q17, [sp, #(112 +  8 * 32)]  // Save v16, v17
    stp     q18, q19, [sp, #(112 +  9 * 32)]  // Save v18, v19
    stp     q20, q21, [sp, #(112 + 10 * 32)]  // Save v20, v21
    stp     q22, q23, [sp, #(112 + 11 * 32)]  // Save v22, v23
    stp     q24, q25, [sp, #(112 + 12 * 32)]  // Save v24, v25
    stp     q26, q27, [sp, #(112 + 13 * 32)]  // Save v26, v27
    stp     q28, q29, [sp, #(112 + 14 * 32)]  // Save v28, v29
    stp     q30, q31, [sp, #(112 + 15 * 32)]  // Save v30, v31

    // --- Store current SP into old thread's TCB ---
    mov     x2, sp                       // x2 = current SP
    str     x2, [x0]                     // old_tcb.saved_sp = SP

    // --- Load new thread's SP from its TCB ---
    ldr     x2, [x1]                     // x2 = new_tcb.saved_sp
    mov     sp, x2                       // SP = new thread's stack

    // --- Restore v0..v31 (offsets 112..623) ---
    // Mirrors the save sequence — same offsets, ldp instead of stp.
    ldp     q0,  q1,  [sp, #(112 +  0 * 32)]  // Restore v0, v1
    ldp     q2,  q3,  [sp, #(112 +  1 * 32)]  // Restore v2, v3
    ldp     q4,  q5,  [sp, #(112 +  2 * 32)]  // Restore v4, v5
    ldp     q6,  q7,  [sp, #(112 +  3 * 32)]  // Restore v6, v7
    ldp     q8,  q9,  [sp, #(112 +  4 * 32)]  // Restore v8, v9
    ldp     q10, q11, [sp, #(112 +  5 * 32)]  // Restore v10, v11
    ldp     q12, q13, [sp, #(112 +  6 * 32)]  // Restore v12, v13
    ldp     q14, q15, [sp, #(112 +  7 * 32)]  // Restore v14, v15
    ldp     q16, q17, [sp, #(112 +  8 * 32)]  // Restore v16, v17
    ldp     q18, q19, [sp, #(112 +  9 * 32)]  // Restore v18, v19
    ldp     q20, q21, [sp, #(112 + 10 * 32)]  // Restore v20, v21
    ldp     q22, q23, [sp, #(112 + 11 * 32)]  // Restore v22, v23
    ldp     q24, q25, [sp, #(112 + 12 * 32)]  // Restore v24, v25
    ldp     q26, q27, [sp, #(112 + 13 * 32)]  // Restore v26, v27
    ldp     q28, q29, [sp, #(112 + 14 * 32)]  // Restore v28, v29
    ldp     q30, q31, [sp, #(112 + 15 * 32)]  // Restore v30, v31

    // --- Restore FPCR + FPSR (offsets 96..111) ---
    ldp     x2, x3, [sp, #96]            // Load saved fpcr/fpsr as a pair
    msr     FPCR, x2                     // Restore FP control register
    msr     FPSR, x3                     // Restore FP sticky-flag register

    // --- Restore callee-saved GPRs (offsets 0..95) ---
    ldp     x19, x20, [sp, #(0  * 8)]   // Restore x19, x20
    ldp     x21, x22, [sp, #(2  * 8)]   // Restore x21, x22
    ldp     x23, x24, [sp, #(4  * 8)]   // Restore x23, x24
    ldp     x25, x26, [sp, #(6  * 8)]   // Restore x25, x26
    ldp     x27, x28, [sp, #(8  * 8)]   // Restore x27, x28
    ldp     x29, x30, [sp, #(10 * 8)]   // Restore x29 (FP), x30 (LR)
    add     sp, sp, #624                 // Free SavedContext

    ret                                  // "Return" to new thread's LR

// ---------------------------------------------------------------------------
// Thread entry trampoline — new threads "wake up" here.
// x19 holds the entry function pointer, set by the synthetic stack frame.
// ---------------------------------------------------------------------------
.global thread_entry
thread_entry:
    // GKL is held and IRQs are masked (inherited from the scheduling
    // handler that context-switched to us). Each entry function manages
    // its own transition:
    //   - process_entry: releases GKL, drops to EL0 (eret unmasks IRQs)
    //   - kernel threads: run under GKL with IRQs masked (cooperative)
    // The CPU 0 boot TCB never reaches here (no synthetic SavedContext);
    // secondary CPUs have no TCB (park in scheduler::idle_wait instead).
    blr     x19                          // Call the entry function via fn pointer in x19
    // entry function is fn() -> ! so we should never reach here
.Lthread_halt:
    wfi                                  // Halt if entry somehow returns
    b       .Lthread_halt                // Loop forever
"#
);
