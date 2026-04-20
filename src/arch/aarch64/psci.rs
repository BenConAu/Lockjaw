/// PSCI 0.2+ interface for secondary CPU boot (QEMU virt).
///
/// QEMU virt uses HVC as the PSCI conduit. Real hardware may use SMC.
/// Function IDs follow the SMC64 calling convention (ARM DEN0028E).

use core::arch::asm;

/// PSCI function IDs (SMC64 / HVC64)
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Issue a PSCI CPU_ON call to start a secondary core.
///
/// - `target_cpu`: MPIDR value of the core to start (for QEMU virt
///   with linear topology, this is just the CPU index: 0, 1, 2, 3).
/// - `entry_point`: physical address the secondary starts executing at.
/// - `context_id`: value passed in x0 to the secondary's entry point.
///
/// Returns 0 on success, negative PSCI error code on failure.
///
/// # Safety
/// `entry_point` must be a valid physical address of executable code
/// that sets up its own stack and never returns.
pub unsafe fn cpu_on(target_cpu: u64, entry_point: u64, context_id: u64) -> i64 {
    let ret: i64;
    asm!(
        "hvc #0",                            // PSCI conduit for QEMU virt
        inout("x0") PSCI_CPU_ON => ret,      // x0 = function ID, returns status
        in("x1") target_cpu,                 // x1 = target CPU MPIDR
        in("x2") entry_point,               // x2 = entry point (physical)
        in("x3") context_id,                 // x3 = context ID (passed as x0)
    );
    ret
}
