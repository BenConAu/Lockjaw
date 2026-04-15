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
    /// User stack pointer (SP_EL0). Must be saved/restored so that context
    /// switches during IRQs or syscalls don't lose the interrupted thread's
    /// user SP.
    pub sp_el0: u64,
    /// Padding to keep the frame 16-byte aligned (AArch64 ABI requirement).
    pub _pad: u64,
}

/// Frame size must be 16-byte aligned for AArch64 ABI stack alignment.
const _: () = assert!(
    core::mem::size_of::<ExceptionContext>() % 16 == 0,
    "ExceptionContext size must be 16-byte aligned"
);

/// Frame size in bytes, used by the assembly save/restore macros.
pub const EXCEPTION_FRAME_SIZE: usize = core::mem::size_of::<ExceptionContext>();

// Field offsets for assembly. Using core::mem::offset_of! so these stay
// correct if fields are reordered or padding changes.
pub const OFF_ELR: usize = core::mem::offset_of!(ExceptionContext, elr);
pub const OFF_SPSR: usize = core::mem::offset_of!(ExceptionContext, spsr);
pub const OFF_ESR: usize = core::mem::offset_of!(ExceptionContext, esr);
pub const OFF_SP_EL0: usize = core::mem::offset_of!(ExceptionContext, sp_el0);

// ---------------------------------------------------------------------------
// Rust exception handlers (called from assembly stubs)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ESR decode helpers
// ---------------------------------------------------------------------------

fn exception_class_str(ec: u64) -> &'static str {
    match ec {
        0x00 => "Unknown reason",
        0x01 => "Trapped WFI/WFE",
        0x15 => "SVC from AArch64 (syscall)",
        0x18 => "Trapped MSR/MRS/System instruction",
        0x20 => "Instruction Abort from lower EL",
        0x21 => "Instruction Abort from same EL",
        0x22 => "PC alignment fault",
        0x24 => "Data Abort from lower EL",
        0x25 => "Data Abort from same EL",
        0x26 => "SP alignment fault",
        0x2C => "Trapped FP exception",
        0x30 => "Breakpoint from lower EL",
        0x31 => "Breakpoint from same EL",
        0x3C => "BRK instruction",
        _    => "Other/reserved",
    }
}

fn data_fault_str(dfsc: u64) -> &'static str {
    match dfsc & 0x3F {
        0x04 => "Translation fault, level 0",
        0x05 => "Translation fault, level 1",
        0x06 => "Translation fault, level 2",
        0x07 => "Translation fault, level 3",
        0x09 => "Access flag fault, level 1",
        0x0A => "Access flag fault, level 2",
        0x0B => "Access flag fault, level 3",
        0x0D => "Permission fault, level 1",
        0x0E => "Permission fault, level 2",
        0x0F => "Permission fault, level 3",
        0x10 => "Synchronous external abort",
        0x21 => "Alignment fault",
        _    => "Other/reserved DFSC",
    }
}

/// Classify a virtual address into a known memory region.
///
/// These ranges are specific to QEMU virt (aarch64) with our kernel config:
/// - KERNEL higher-half: TTBR1 mapping at 0xFFFF_0000_0000_0000 + phys
///   (set up by enable_higher_half in mmu.rs)
/// - NULL DEREF: first page, always unmapped, catches null pointer dereferences
/// - DEVICE MMIO: 0x0800_0000 - 0x09FF_FFFF covers GIC (0x0800_0000) and
///   UARTs (0x0900_0000, 0x0904_0000). From QEMU virt DTB, verified via
///   `qemu-system-aarch64 -machine virt,dumpdtb=...`
/// - USERSPACE: 0x0000_1000 - 0x3FFF_FFFF, the first 1GB minus the null page.
///   User processes are mapped in this range (code at 0x0040_0000, stack at
///   0x0080_0000). Constrained by L0[0]/L1[0] in create_address_space.
/// - RAM: 0x4000_0000 - 0x47FF_FFFF, 128MB physical RAM on QEMU virt default.
///   RAM_BASE is defined in platform.rs. Kernel is loaded at 0x4020_0000.
fn classify_address(addr: u64) -> &'static str {
    if addr >= 0xFFFF_0000_0000_0000 {
        "KERNEL higher-half"
    } else if addr < 0x1000 {
        "NULL DEREF (near-zero)"
    } else if addr >= 0x0800_0000 && addr < 0x0A00_0000 {
        // GIC at 0x0800_0000, UARTs at 0x0900_0000/0x0904_0000
        // (QEMU virt DTB: intc@8000000, pl011@9000000, pl011@9040000)
        "DEVICE MMIO region"
    } else if addr < 0x4000_0000 {
        // First 1GB: user process virtual address space
        "USERSPACE"
    } else if addr < 0x4800_0000 {
        // 0x4000_0000 - 0x47FF_FFFF: 128MB RAM (QEMU virt default)
        "RAM (physical range)"
    } else {
        "NON-CANONICAL / unmapped gap"
    }
}

// ---------------------------------------------------------------------------
// Structured crash output
// ---------------------------------------------------------------------------

fn print_fault(prefix: &str, ctx: &ExceptionContext, is_user: bool) {
    let esr = ctx.esr;
    let ec = (esr >> 26) & 0x3F;
    let far: u64;
    unsafe { core::arch::asm!("mrs {}, FAR_EL1", out(reg) far) };

    crate::kprintln!("========================================");
    crate::kprintln!("{}  HARDWARE EXCEPTION", prefix);

    // Kernel stack canary check — if corrupted, register dump is unreliable
    crate::mm::stack::check_canary_report(prefix);

    // Thread identification and syscall breadcrumb
    crate::crash::print_thread_context(prefix);

    crate::kprintln!("{}  ESR:  {:#010x} — {} — {}", prefix, esr,
        exception_class_str(ec), data_fault_str(esr));
    crate::kprintln!("{}  ELR:  {:#018x} [{}]", prefix, ctx.elr, classify_address(ctx.elr));
    crate::kprintln!("{}  FAR:  {:#018x} [{}]", prefix, far, classify_address(far));
    crate::kprintln!("{}  SPSR: {:#018x}", prefix, ctx.spsr);
    if is_user {
        crate::kprintln!("{}  SP_EL0: {:#018x}", prefix, ctx.sp_el0);
    }

    // User stack overflow detection
    if is_user && (ec == 0x24) && (esr & 0x3F) >= 0x04 && (esr & 0x3F) <= 0x07 {
        // Data abort from lower EL with translation fault — check if near stack
        unsafe {
            let tcb_paddr = crate::sched::scheduler::current_tcb_paddr();
            // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
            let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET)
                as *const crate::sched::tcb::Tcb;
            let stack_base = (*tcb).user_stack_base;
            let stack_top = (*tcb).user_stack_top;
            if stack_base != 0 {
                crate::kprintln!("{}  Stack: {:#x} - {:#x} ({} bytes)",
                    prefix, stack_base, stack_top, stack_top - stack_base);
                // Check below stack base (normal downward overflow)
                if far < stack_base && far >= stack_base.saturating_sub(crate::mm::addr::PAGE_SIZE) {
                    crate::kprintln!("{}  *** USER STACK OVERFLOW DETECTED ***", prefix);
                    crate::kprintln!("{}  Overflowed by {} bytes (below stack base)", prefix, stack_base - far);
                }
                // Check above stack top (abnormal, but still unmapped for small stacks)
                if far >= stack_top && far < stack_top + crate::mm::addr::PAGE_SIZE {
                    crate::kprintln!("{}  *** USER STACK OVERFLOW DETECTED ***", prefix);
                    crate::kprintln!("{}  Fault {} bytes above stack top", prefix, far - stack_top);
                }
            }
        }
    }

    // Dump first 16 GPRs
    for i in 0..4 {
        crate::kprintln!("{}  x{:02}={:#018x}  x{:02}={:#018x}  x{:02}={:#018x}  x{:02}={:#018x}",
            prefix,
            i*4, ctx.gpr[i*4], i*4+1, ctx.gpr[i*4+1],
            i*4+2, ctx.gpr[i*4+2], i*4+3, ctx.gpr[i*4+3]);
    }

    crate::kprintln!("========================================");
}

// ---------------------------------------------------------------------------
// Rust exception handlers (called from assembly stubs)
// ---------------------------------------------------------------------------

/// Detected ELR=0 in the exception context after a syscall handler returned.
/// This means something in the kernel corrupted the saved return address.
/// Print diagnostics and halt — this is a kernel bug, not a user fault.
#[no_mangle]
extern "C" fn handle_elr_corruption(ctx: &ExceptionContext) {
    crate::kprintln!("========================================");
    crate::kprintln!("[BUG:KERN]  ELR CORRUPTED TO ZERO DURING SYSCALL");
    crate::crash::print_thread_context("[BUG:KERN]");
    crate::mm::stack::check_canary_report("[BUG:KERN]");
    // Print SP to see how much kernel stack was used
    let sp: u64;
    unsafe { core::arch::asm!("mov {}, sp", out(reg) sp); }
    crate::kprintln!("[BUG:KERN]  Kernel SP: {:#018x}", sp);
    // SAFETY: printing address of exception context on kernel stack
    crate::kprintln!("[BUG:KERN]  Exception frame at: {:#018x}", ctx as *const _ as u64);
    // Print the GPRs from the exception context
    for i in 0..16 {
        crate::kprintln!("[BUG:KERN]  x{:02}={:#018x}  x{:02}={:#018x}",
            i*2, ctx.gpr[i*2], i*2+1, ctx.gpr[i*2+1]);
    }
    crate::kprintln!("========================================");
    loop { unsafe { core::arch::asm!("wfi"); } }
}

/// Synchronous exception from same EL (kernel fault).
#[no_mangle]
extern "C" fn handle_exception_sync(ctx: &ExceptionContext) {
    print_fault("[FAULT:KERN]", ctx, false);
    loop { unsafe { core::arch::asm!("wfi") }; }
}

/// Synchronous exception from lower EL (userspace).
/// Dispatches SVC (syscalls) and prints faults.
#[no_mangle]
extern "C" fn handle_exception_sync_lower(ctx: &mut ExceptionContext) {
    let ec = (ctx.esr >> 26) & 0x3F;

    match ec {
        0x15 => {
            // SVC from AArch64 — syscall dispatch
            crate::syscall::handler::handle_syscall(ctx);
        }
        _ => {
            // Userspace fault
            print_fault("[FAULT:USER]", ctx, true);
            loop { unsafe { core::arch::asm!("wfi") }; }
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
extern "C" fn handle_exception_fiq(ctx: &ExceptionContext) {
    print_fault("[FAULT:KERN]", ctx, false);
    loop { unsafe { core::arch::asm!("wfi") }; }
}

#[no_mangle]
extern "C" fn handle_exception_serror(ctx: &ExceptionContext) {
    print_fault("[FAULT:KERN]", ctx, false);
    loop { unsafe { core::arch::asm!("wfi") }; }
}

// ---------------------------------------------------------------------------
// Vector table assembly
// ---------------------------------------------------------------------------

global_asm!(
    r#"
// ---------------------------------------------------------------------------
// Register save/restore macros
// ---------------------------------------------------------------------------
// Frame layout offsets and size are set by .equ from Rust consts below.
// This ensures the assembly always matches the ExceptionContext struct.

.equ FRAME_SIZE, {frame_size}
.equ OFF_ELR,    {off_elr}
.equ OFF_SPSR,   {off_spsr}
.equ OFF_ESR,    {off_esr}
.equ OFF_SP_EL0, {off_sp_el0}

// Save all registers + ELR/SPSR/ESR/SP_EL0 onto the stack.
// Creates an ExceptionContext struct on the stack and passes its
// address in x0 to the Rust handler.
.macro SAVE_REGS
    sub     sp, sp, #FRAME_SIZE

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
    mrs     x3, SP_EL0                   // Read user stack pointer

    stp     x0, x1,   [sp, #OFF_ELR]    // Save ELR, SPSR
    stp     x2, x3,   [sp, #OFF_ESR]    // Save ESR, SP_EL0

    mov     x0, sp                       // x0 = pointer to ExceptionContext (arg for handler)
.endm

// Restore all registers from the ExceptionContext on the stack
// and return from exception.
.macro RESTORE_REGS
    ldp     x0, x1,   [sp, #OFF_ELR]    // Load saved ELR, SPSR
    msr     ELR_EL1, x0                  // Restore Exception Link Register
    msr     SPSR_EL1, x1                 // Restore Saved Program Status Register

    ldp     x2, x3,   [sp, #OFF_ESR]    // Load saved ESR (unused), SP_EL0
    msr     SP_EL0, x3                   // Restore user stack pointer

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

    add     sp, sp, #FRAME_SIZE          // Free the stack frame

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
    // Check if ELR was corrupted to 0 during the handler
    ldr     x1, [sp, #OFF_ELR]          // Load saved ELR from exception context
    cbnz    x1, 1f                       // If nonzero, proceed to normal restore
    // ELR is 0 — something corrupted it during the syscall
    mov     x0, sp                       // x0 = &ExceptionContext for the fault handler
    bl      handle_elr_corruption        // Call diagnostic (does not return)
1:
    RESTORE_REGS                         // Restore and eret back to EL0
"#,
    frame_size = const EXCEPTION_FRAME_SIZE,
    off_elr = const OFF_ELR,
    off_spsr = const OFF_SPSR,
    off_esr = const OFF_ESR,
    off_sp_el0 = const OFF_SP_EL0,
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
    // SAFETY: linker symbol
    let vbar = &raw const __exception_vectors as u64;
    core::arch::asm!(
        "msr VBAR_EL1, {addr}",             // Set Vector Base Address Register
        "isb",                               // Sync pipeline after VBAR write
        addr = in(reg) vbar,
    );
}
