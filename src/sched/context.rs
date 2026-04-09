use core::arch::global_asm;

// SavedContext layout (12 x u64 = 96 bytes, matches assembly below):
//   [0]  x19   [1]  x20   [2]  x21   [3]  x22
//   [4]  x23   [5]  x24   [6]  x25   [7]  x26
//   [8]  x27   [9]  x28   [10] x29   [11] LR (x30)

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
.global context_switch
context_switch:
    // x0 = &old_tcb.saved_sp, x1 = &new_tcb.saved_sp

    // --- Save callee-saved registers on current stack ---
    sub     sp, sp, #(12 * 8)           // Allocate SavedContext (12 regs x 8 bytes)
    stp     x19, x20, [sp, #(0  * 8)]  // Save x19, x20
    stp     x21, x22, [sp, #(2  * 8)]  // Save x21, x22
    stp     x23, x24, [sp, #(4  * 8)]  // Save x23, x24
    stp     x25, x26, [sp, #(6  * 8)]  // Save x25, x26
    stp     x27, x28, [sp, #(8  * 8)]  // Save x27, x28
    stp     x29, x30, [sp, #(10 * 8)]  // Save x29 (FP), x30 (LR)

    // --- Store current SP into old thread's TCB ---
    mov     x2, sp                      // x2 = current SP
    str     x2, [x0]                    // old_tcb.saved_sp = SP

    // --- Load new thread's SP from its TCB ---
    ldr     x2, [x1]                    // x2 = new_tcb.saved_sp
    mov     sp, x2                      // SP = new thread's stack

    // --- Restore callee-saved registers from new stack ---
    ldp     x19, x20, [sp, #(0  * 8)]  // Restore x19, x20
    ldp     x21, x22, [sp, #(2  * 8)]  // Restore x21, x22
    ldp     x23, x24, [sp, #(4  * 8)]  // Restore x23, x24
    ldp     x25, x26, [sp, #(6  * 8)]  // Restore x25, x26
    ldp     x27, x28, [sp, #(8  * 8)]  // Restore x27, x28
    ldp     x29, x30, [sp, #(10 * 8)]  // Restore x29 (FP), x30 (LR)
    add     sp, sp, #(12 * 8)           // Free SavedContext

    ret                                  // "Return" to new thread's LR

// ---------------------------------------------------------------------------
// Thread entry trampoline — new threads "wake up" here.
// x19 holds the entry function pointer, set by the synthetic stack frame.
// ---------------------------------------------------------------------------
.global thread_entry
thread_entry:
    msr     DAIFClr, #2                 // Unmask IRQ (new threads start with IRQs masked)
    blr     x19                          // Call the entry function via fn pointer in x19
    // entry function is fn() -> ! so we should never reach here
.Lthread_halt:
    wfi                                  // Halt if entry somehow returns
    b       .Lthread_halt                // Loop forever
"#
);
