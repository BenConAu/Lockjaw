use core::arch::global_asm;

/// Exception context saved by the vector entry stub.
/// Must match the layout in the assembly save/restore macros.
#[repr(C)]
/// CPU state saved by the exception vector entry stub.
///
/// Created on the kernel stack by the SAVE_REGS assembly macro when an
/// exception is taken. The Rust handler receives a pointer to this struct.
/// For syscalls, the handler modifies `gpr[0]` (x0) to set the return value.
/// RESTORE_REGS loads the (potentially modified) values back before `eret`.
///
/// Layout must match the assembly in SAVE_REGS/RESTORE_REGS exactly.
pub struct ExceptionContext {
    /// General-purpose registers x0–x30.
    pub gpr: [u64; 31],
    /// Exception Link Register — the PC to return to.
    pub elr: u64,
    /// Saved Program Status Register (PSTATE at time of exception).
    pub spsr: u64,
    /// Exception Syndrome Register — encodes the exception cause.
    /// EC field (bits 31:26) identifies the exception class (SVC, data abort, etc).
    pub esr: u64,
}

// ---------------------------------------------------------------------------
// Rust exception handlers (called from assembly stubs)
// ---------------------------------------------------------------------------

#[no_mangle]
extern "C" fn handle_exception_sync(ctx: &ExceptionContext) {
    let esr = ctx.esr;
    let ec = (esr >> 26) & 0x3F; // Exception Class
    let iss = esr & 0x01FF_FFFF; // Instruction Specific Syndrome

    crate::kprintln!();
    crate::kprintln!("!!! EXCEPTION: Synchronous !!!");
    crate::kprintln!("  ELR_EL1:  {:#018x}", ctx.elr);
    crate::kprintln!("  ESR_EL1:  {:#018x} (EC={:#04x} ISS={:#07x})", esr, ec, iss);
    crate::kprintln!("  SPSR_EL1: {:#018x}", ctx.spsr);
    let far: u64;
    unsafe { core::arch::asm!("mrs {}, FAR_EL1", out(reg) far) };
    crate::kprintln!("  FAR_EL1:  {:#018x}", far);

    match ec {
        0x00 => crate::kprintln!("  Cause: Unknown reason"),
        0x15 => crate::kprintln!("  Cause: SVC (syscall) from AArch64"),
        0x20 => crate::kprintln!("  Cause: Instruction abort, lower EL"),
        0x21 => crate::kprintln!("  Cause: Instruction abort, same EL"),
        0x24 => crate::kprintln!("  Cause: Data abort, lower EL"),
        0x25 => crate::kprintln!("  Cause: Data abort, same EL"),
        _ => crate::kprintln!("  Cause: EC={:#04x}", ec),
    }

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// Synchronous exception from lower EL (userspace).
/// Dispatches SVC (syscalls) and prints faults.
#[no_mangle]
extern "C" fn handle_exception_sync_lower(ctx: &mut ExceptionContext) {
    let esr = ctx.esr;
    let ec = (esr >> 26) & 0x3F;

    match ec {
        0x15 => {
            // SVC from AArch64 — syscall dispatch
            crate::syscall::handler::handle_syscall(ctx);
        }
        _ => {
            // Userspace fault — print details and halt
            let iss = esr & 0x01FF_FFFF;
            crate::kprintln!();
            crate::kprintln!("!!! USERSPACE FAULT !!!");
            crate::kprintln!("  ELR_EL1:  {:#018x}", ctx.elr);
            crate::kprintln!("  ESR_EL1:  {:#018x} (EC={:#04x} ISS={:#07x})", esr, ec, iss);
            let far: u64;
            unsafe { core::arch::asm!("mrs {}, FAR_EL1", out(reg) far) };
            crate::kprintln!("  FAR_EL1:  {:#018x}", far);
            loop {
                unsafe { core::arch::asm!("wfi") };
            }
        }
    }
}

#[no_mangle]
extern "C" fn handle_exception_irq(ctx: &ExceptionContext) {
    // Dispatch to the IRQ handler (GIC + timer, added in 3.2/3.3)
    crate::arch::aarch64::irq_dispatch();
    let _ = ctx; // context available if needed later
}

#[no_mangle]
extern "C" fn handle_exception_fiq(_ctx: &ExceptionContext) {
    crate::kprintln!("!!! EXCEPTION: FIQ (unexpected) !!!");
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[no_mangle]
extern "C" fn handle_exception_serror(ctx: &ExceptionContext) {
    crate::kprintln!("!!! EXCEPTION: SError !!!");
    crate::kprintln!("  ELR_EL1:  {:#018x}", ctx.elr);
    crate::kprintln!("  ESR_EL1:  {:#018x}", ctx.esr);
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

// ---------------------------------------------------------------------------
// Vector table assembly
// ---------------------------------------------------------------------------

global_asm!(
    r#"
// ---------------------------------------------------------------------------
// Register save/restore macros
// ---------------------------------------------------------------------------

// Save all caller-saved registers + ELR/SPSR/ESR onto the stack.
// Creates an ExceptionContext struct on the stack and passes its
// address in x0 to the Rust handler.
.macro SAVE_REGS
    sub     sp, sp, #(34 * 8)            // Allocate stack frame: 31 GPR + ELR + SPSR + ESR

    stp     x0,  x1,  [sp, #(0  * 8)]   // Save x0, x1
    stp     x2,  x3,  [sp, #(2  * 8)]   // Save x2, x3
    stp     x4,  x5,  [sp, #(4  * 8)]   // Save x4, x5
    stp     x6,  x7,  [sp, #(6  * 8)]   // Save x6, x7
    stp     x8,  x9,  [sp, #(8  * 8)]   // Save x8, x9
    stp     x10, x11, [sp, #(10 * 8)]   // Save x10, x11
    stp     x12, x13, [sp, #(12 * 8)]   // Save x12, x13
    stp     x14, x15, [sp, #(14 * 8)]   // Save x14, x15
    stp     x16, x17, [sp, #(16 * 8)]   // Save x16, x17
    stp     x18, x19, [sp, #(18 * 8)]   // Save x18, x19
    stp     x20, x21, [sp, #(20 * 8)]   // Save x20, x21
    stp     x22, x23, [sp, #(22 * 8)]   // Save x22, x23
    stp     x24, x25, [sp, #(24 * 8)]   // Save x24, x25
    stp     x26, x27, [sp, #(26 * 8)]   // Save x26, x27
    stp     x28, x29, [sp, #(28 * 8)]   // Save x28, x29 (frame pointer)
    str     x30,      [sp, #(30 * 8)]   // Save x30 (link register)

    mrs     x0, ELR_EL1                  // Read Exception Link Register
    mrs     x1, SPSR_EL1                 // Read Saved Program Status Register
    mrs     x2, ESR_EL1                  // Read Exception Syndrome Register

    stp     x0, x1,   [sp, #(31 * 8)]   // Save ELR, SPSR
    str     x2,       [sp, #(33 * 8)]   // Save ESR

    mov     x0, sp                       // x0 = pointer to ExceptionContext (arg for handler)
.endm

// Restore all registers from the ExceptionContext on the stack
// and return from exception.
.macro RESTORE_REGS
    ldp     x0, x1,   [sp, #(31 * 8)]   // Load saved ELR, SPSR
    msr     ELR_EL1, x0                  // Restore Exception Link Register
    msr     SPSR_EL1, x1                 // Restore Saved Program Status Register

    ldp     x0,  x1,  [sp, #(0  * 8)]   // Restore x0, x1
    ldp     x2,  x3,  [sp, #(2  * 8)]   // Restore x2, x3
    ldp     x4,  x5,  [sp, #(4  * 8)]   // Restore x4, x5
    ldp     x6,  x7,  [sp, #(6  * 8)]   // Restore x6, x7
    ldp     x8,  x9,  [sp, #(8  * 8)]   // Restore x8, x9
    ldp     x10, x11, [sp, #(10 * 8)]   // Restore x10, x11
    ldp     x12, x13, [sp, #(12 * 8)]   // Restore x12, x13
    ldp     x14, x15, [sp, #(14 * 8)]   // Restore x14, x15
    ldp     x16, x17, [sp, #(16 * 8)]   // Restore x16, x17
    ldp     x18, x19, [sp, #(18 * 8)]   // Restore x18, x19
    ldp     x20, x21, [sp, #(20 * 8)]   // Restore x20, x21
    ldp     x22, x23, [sp, #(22 * 8)]   // Restore x22, x23
    ldp     x24, x25, [sp, #(24 * 8)]   // Restore x24, x25
    ldp     x26, x27, [sp, #(26 * 8)]   // Restore x26, x27
    ldp     x28, x29, [sp, #(28 * 8)]   // Restore x28, x29
    ldr     x30,      [sp, #(30 * 8)]   // Restore x30

    add     sp, sp, #(34 * 8)            // Free the stack frame

    eret                                  // Return from exception
.endm

// ---------------------------------------------------------------------------
// Vector table stubs — each entry is 128 bytes, containing only a branch
// to the full handler outside the table.
// ---------------------------------------------------------------------------
.macro VECTOR_STUB target
    .balign 128                          // Each vector entry must be 128-byte aligned
    b       \target                      // Branch to full handler (save/call/restore)
.endm

// ---------------------------------------------------------------------------
// Exception vector table
// Must be 2 KB (0x800) aligned. Loaded into VBAR_EL1.
// 16 entries: 4 groups (source) x 4 types (sync/irq/fiq/serror)
// ---------------------------------------------------------------------------
.section .text.vectors, "ax"
.balign 0x800                            // 2 KB alignment required by VBAR_EL1
.global __exception_vectors
__exception_vectors:

    // --- Current EL, SP_EL0 (not used — we always use SP_ELx) ---
    VECTOR_STUB __vec_sync               // 0x000: Synchronous
    VECTOR_STUB __vec_irq                // 0x080: IRQ
    VECTOR_STUB __vec_fiq                // 0x100: FIQ
    VECTOR_STUB __vec_serror             // 0x180: SError

    // --- Current EL, SP_ELx (kernel-mode exceptions) ---
    VECTOR_STUB __vec_sync               // 0x200: Synchronous
    VECTOR_STUB __vec_irq                // 0x280: IRQ
    VECTOR_STUB __vec_fiq                // 0x300: FIQ
    VECTOR_STUB __vec_serror             // 0x380: SError

    // --- Lower EL, AArch64 (userspace exceptions — Phase 6) ---
    VECTOR_STUB __vec_sync_lower         // 0x400: Synchronous (syscalls land here)
    VECTOR_STUB __vec_irq                // 0x480: IRQ (timer works the same)
    VECTOR_STUB __vec_fiq                // 0x500: FIQ
    VECTOR_STUB __vec_serror             // 0x580: SError

    // --- Lower EL, AArch32 (not supported) ---
    VECTOR_STUB __vec_sync               // 0x600: Synchronous
    VECTOR_STUB __vec_irq                // 0x680: IRQ
    VECTOR_STUB __vec_fiq                // 0x700: FIQ
    VECTOR_STUB __vec_serror             // 0x780: SError

// ---------------------------------------------------------------------------
// Full handlers — outside the vector table, no size constraint.
// Each one: save regs, call Rust handler, restore regs, eret.
// ---------------------------------------------------------------------------
__vec_sync:
    SAVE_REGS                            // Save all registers, x0 = &ExceptionContext
    bl      handle_exception_sync        // Call Rust synchronous exception handler
    RESTORE_REGS                         // Restore registers and eret

__vec_irq:
    SAVE_REGS                            // Save all registers
    bl      handle_exception_irq         // Call Rust IRQ handler
    RESTORE_REGS                         // Restore and eret

__vec_fiq:
    SAVE_REGS                            // Save all registers
    bl      handle_exception_fiq         // Call Rust FIQ handler
    RESTORE_REGS                         // Restore and eret

__vec_serror:
    SAVE_REGS                            // Save all registers
    bl      handle_exception_serror      // Call Rust SError handler
    RESTORE_REGS                         // Restore and eret

__vec_sync_lower:
    SAVE_REGS                            // Save all registers, x0 = &mut ExceptionContext
    bl      handle_exception_sync_lower  // Call lower-EL sync handler (syscall dispatch)
    RESTORE_REGS                         // Restore and eret back to EL0
"#
);

/// Install the exception vector table by writing VBAR_EL1.
///
/// # Safety
/// Must be called once during boot. The vector table must be in mapped memory.
/// Install the exception vector table by writing VBAR_EL1.
///
/// After this call, all exceptions (synchronous, IRQ, FIQ, SError) from
/// both EL1 and EL0 are routed through the vector table in `__exception_vectors`.
///
/// # Safety
/// Must be called once during boot. The vector table must be in mapped memory.
pub unsafe fn init() {
    extern "C" {
        static __exception_vectors: u8;
    }
    let vbar = &raw const __exception_vectors as u64;
    core::arch::asm!(
        "msr VBAR_EL1, {addr}",             // Set Vector Base Address Register
        "isb",                               // Sync pipeline after VBAR write
        addr = in(reg) vbar,
    );
}
