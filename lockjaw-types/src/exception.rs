/// Exception frame layout and ESR classification — the contract
/// between Rust and the exception vector assembly.
///
/// ExceptionContext defines the register save frame that SAVE_REGS
/// pushes onto the kernel stack. Its field layout determines the
/// assembly offsets in SAVE_REGS/RESTORE_REGS. This struct lives in
/// lockjaw-types so layout invariants are host-testable — same
/// pattern as SavedContext and Tcb.
///
/// ESR decode functions are pure classification logic: extract
/// exception class, data fault status, etc. The kernel uses
/// classify_sync_exception() as a pull decision instead of
/// branching on raw ESR bits inline.

// ---------------------------------------------------------------------------
// ExceptionContext — register save frame
// ---------------------------------------------------------------------------

/// CPU state saved by the exception vector entry stub.
///
/// Created on the kernel stack by the SAVE_REGS assembly macro when an
/// exception is taken. The Rust handler receives a pointer to this struct.
/// For syscalls, the handler modifies `gpr[0]` (x0) to set the return value.
/// RESTORE_REGS loads the (potentially modified) values back before `eret`.
///
/// Layout must match the assembly in SAVE_REGS/RESTORE_REGS exactly.
#[repr(C)]
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

/// Frame size for SAVE_REGS: `sub sp, sp, #EXCEPTION_FRAME_SIZE`.
pub const EXCEPTION_FRAME_SIZE: usize = core::mem::size_of::<ExceptionContext>();

/// Field offsets for assembly. The assembly template uses these via
/// `.equ OFF_ELR, {off_elr}` etc.
pub const OFF_ELR: usize = core::mem::offset_of!(ExceptionContext, elr);
pub const OFF_SPSR: usize = core::mem::offset_of!(ExceptionContext, spsr);
pub const OFF_ESR: usize = core::mem::offset_of!(ExceptionContext, esr);
pub const OFF_SP_EL0: usize = core::mem::offset_of!(ExceptionContext, sp_el0);

// Compile-time assertions tying struct layout to the assembly.
const _: () = {
    assert!(core::mem::size_of::<ExceptionContext>() == 288);
    assert!(core::mem::offset_of!(ExceptionContext, elr) == 248);
    assert!(core::mem::offset_of!(ExceptionContext, spsr) == 256);
    assert!(core::mem::offset_of!(ExceptionContext, esr) == 264);
    assert!(core::mem::offset_of!(ExceptionContext, sp_el0) == 272);
};

// ---------------------------------------------------------------------------
// ESR decode — pure classification
// ---------------------------------------------------------------------------

/// Extract the Exception Class from ESR_EL1 (bits 31:26).
pub const fn esr_exception_class(esr: u64) -> u8 {
    ((esr >> 26) & 0x3F) as u8
}

/// Extract the Data Fault Status Code from ESR (bits 5:0).
pub const fn esr_data_fault_status(esr: u64) -> u8 {
    (esr & 0x3F) as u8
}

// Exception class constants (EC field values).
pub const EC_UNKNOWN: u8 = 0x00;
pub const EC_TRAPPED_WFI_WFE: u8 = 0x01;
pub const EC_SVC_AARCH64: u8 = 0x15;
pub const EC_TRAPPED_MSR_MRS: u8 = 0x18;
pub const EC_INSTRUCTION_ABORT_LOWER: u8 = 0x20;
pub const EC_INSTRUCTION_ABORT_SAME: u8 = 0x21;
pub const EC_PC_ALIGNMENT: u8 = 0x22;
pub const EC_DATA_ABORT_LOWER: u8 = 0x24;
pub const EC_DATA_ABORT_SAME: u8 = 0x25;
pub const EC_SP_ALIGNMENT: u8 = 0x26;
pub const EC_TRAPPED_FP: u8 = 0x2C;
pub const EC_BREAKPOINT_LOWER: u8 = 0x30;
pub const EC_BREAKPOINT_SAME: u8 = 0x31;
pub const EC_BRK_INSTRUCTION: u8 = 0x3C;

/// Human-readable name for an exception class.
pub fn exception_class_name(ec: u8) -> &'static str {
    match ec {
        EC_UNKNOWN => "Unknown reason",
        EC_TRAPPED_WFI_WFE => "Trapped WFI/WFE",
        EC_SVC_AARCH64 => "SVC from AArch64 (syscall)",
        EC_TRAPPED_MSR_MRS => "Trapped MSR/MRS/System instruction",
        EC_INSTRUCTION_ABORT_LOWER => "Instruction Abort from lower EL",
        EC_INSTRUCTION_ABORT_SAME => "Instruction Abort from same EL",
        EC_PC_ALIGNMENT => "PC alignment fault",
        EC_DATA_ABORT_LOWER => "Data Abort from lower EL",
        EC_DATA_ABORT_SAME => "Data Abort from same EL",
        EC_SP_ALIGNMENT => "SP alignment fault",
        EC_TRAPPED_FP => "Trapped FP exception",
        EC_BREAKPOINT_LOWER => "Breakpoint from lower EL",
        EC_BREAKPOINT_SAME => "Breakpoint from same EL",
        EC_BRK_INSTRUCTION => "BRK instruction",
        _ => "Other/reserved",
    }
}

/// Human-readable name for a Data Fault Status Code (DFSC, ESR bits 5:0).
pub fn data_fault_name(dfsc: u8) -> &'static str {
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
        _ => "Other/reserved DFSC",
    }
}

// ---------------------------------------------------------------------------
// Sync exception classification — pull decision
// ---------------------------------------------------------------------------

/// Decision for handling a synchronous exception from lower EL (userspace).
/// The kernel matches on this instead of branching on raw ESR bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncExceptionAction {
    /// SVC from AArch64 — dispatch as syscall.
    Syscall,
    /// Userspace fault — print diagnostics and halt.
    UserFault,
}

/// Classify a synchronous exception from the raw ESR value.
pub fn classify_sync_exception(esr: u64) -> SyncExceptionAction {
    match esr_exception_class(esr) {
        EC_SVC_AARCH64 => SyncExceptionAction::Syscall,
        _ => SyncExceptionAction::UserFault,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Layout (assembly ABI contract — exact numbers) ---

    #[test]
    fn exception_context_size_is_288_bytes() {
        assert_eq!(core::mem::size_of::<ExceptionContext>(), 288);
    }

    #[test]
    fn exception_context_elr_at_offset_248() {
        // WHY: SAVE_REGS assembly stores ELR at [sp, #OFF_ELR].
        // If this offset changes, the assembly silently saves/restores
        // the wrong field.
        assert_eq!(core::mem::offset_of!(ExceptionContext, elr), 248);
    }

    #[test]
    fn exception_context_spsr_at_offset_256() {
        // WHY: SAVE_REGS stores SPSR at [sp, #OFF_SPSR].
        assert_eq!(core::mem::offset_of!(ExceptionContext, spsr), 256);
    }

    #[test]
    fn exception_context_esr_at_offset_264() {
        // WHY: SAVE_REGS stores ESR at [sp, #OFF_ESR].
        assert_eq!(core::mem::offset_of!(ExceptionContext, esr), 264);
    }

    #[test]
    fn exception_context_sp_el0_at_offset_272() {
        // WHY: SAVE_REGS stores SP_EL0 at [sp, #OFF_SP_EL0].
        // RESTORE_REGS reads it back and writes MSR SP_EL0.
        assert_eq!(core::mem::offset_of!(ExceptionContext, sp_el0), 272);
    }

    #[test]
    fn exception_frame_size_matches_struct() {
        assert_eq!(EXCEPTION_FRAME_SIZE, core::mem::size_of::<ExceptionContext>());
    }

    // --- ESR decode ---

    #[test]
    fn esr_ec_extraction() {
        // EC is bits 31:26. SVC = 0x15 = 0b010101.
        // ESR with EC=0x15: 0x15 << 26 = 0x5400_0000
        let esr = 0x5600_0001u64; // EC=0x15, ISS has some bits set
        assert_eq!(esr_exception_class(esr), EC_SVC_AARCH64);
    }

    #[test]
    fn ec_svc_is_0x15() {
        assert_eq!(EC_SVC_AARCH64, 0x15);
    }

    #[test]
    fn ec_data_abort_lower_is_0x24() {
        assert_eq!(EC_DATA_ABORT_LOWER, 0x24);
    }

    #[test]
    fn exception_class_name_svc() {
        assert_eq!(exception_class_name(EC_SVC_AARCH64), "SVC from AArch64 (syscall)");
    }

    #[test]
    fn exception_class_name_unknown_value() {
        assert_eq!(exception_class_name(0xFF), "Other/reserved");
    }

    #[test]
    fn data_fault_name_translation_l3() {
        assert_eq!(data_fault_name(0x07), "Translation fault, level 3");
    }

    #[test]
    fn data_fault_name_permission_l2() {
        assert_eq!(data_fault_name(0x0E), "Permission fault, level 2");
    }

    // --- Sync exception classification ---

    #[test]
    fn classify_svc_is_syscall() {
        let esr = (EC_SVC_AARCH64 as u64) << 26;
        assert_eq!(classify_sync_exception(esr), SyncExceptionAction::Syscall);
    }

    #[test]
    fn classify_data_abort_is_user_fault() {
        let esr = (EC_DATA_ABORT_LOWER as u64) << 26;
        assert_eq!(classify_sync_exception(esr), SyncExceptionAction::UserFault);
    }

    #[test]
    fn classify_unknown_ec_is_user_fault() {
        let esr = 0u64; // EC=0 = unknown
        assert_eq!(classify_sync_exception(esr), SyncExceptionAction::UserFault);
    }
}
