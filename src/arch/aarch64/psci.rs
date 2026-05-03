/// PSCI 0.2+ interface for secondary CPU boot.
///
/// The conduit (HVC vs SMC) is determined at runtime from the DTB
/// `/psci` node's `method` property. Function IDs follow the SMC64
/// calling convention (ARM DEN0028E).

use core::arch::asm;

/// PSCI function IDs (SMC64 / HVC64)
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Issue a PSCI CPU_ON call to start a secondary core.
///
/// - `target_cpu`: MPIDR value of the core to start.
/// - `entry_point`: physical address the secondary starts executing at.
/// - `context_id`: value passed in x0 to the secondary's entry point.
/// - `hvc`: true = HVC conduit (e.g. QEMU virt), false = SMC conduit.
///
/// Returns 0 on success, negative PSCI error code on failure.
///
/// # Safety
/// `entry_point` must be a valid physical address of executable code
/// that sets up its own stack and never returns.
pub unsafe fn cpu_on(target_cpu: u64, entry_point: u64, context_id: u64, hvc: bool) -> i64 {
    let ret: i64;
    // Two asm blocks because the mnemonic (hvc vs smc) is not a
    // runtime operand — it must be a literal in the asm template.
    if hvc {
        asm!(
            "hvc #0",                        // PSCI conduit: hypervisor call
            inout("x0") PSCI_CPU_ON => ret,  // x0 = function ID, returns status
            in("x1") target_cpu,             // x1 = target CPU MPIDR
            in("x2") entry_point,            // x2 = entry point (physical)
            in("x3") context_id,             // x3 = context ID (passed as x0)
        );
    } else {
        asm!(
            "smc #0",                        // PSCI conduit: secure monitor call
            inout("x0") PSCI_CPU_ON => ret,  // x0 = function ID, returns status
            in("x1") target_cpu,             // x1 = target CPU MPIDR
            in("x2") entry_point,            // x2 = entry point (physical)
            in("x3") context_id,             // x3 = context ID (passed as x0)
        );
    }
    ret
}
