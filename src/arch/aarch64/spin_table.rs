/// Spin-table SMP boot for platforms without PSCI (e.g. Pi 4B).
///
/// The firmware stub has each secondary CPU in a WFE loop, polling its
/// release address. Writing a non-zero entry point and issuing SEV
/// wakes the core. The caller must issue SEV after all writes.

use core::arch::asm;

/// Write the entry point to a secondary CPU's release address.
///
/// DSB SY ensures the write is visible to other cores before SEV.
/// The caller MUST issue SEV after all write_release_addr calls.
///
/// # Safety
/// `release_addr` must be a valid physical address from the DTB
/// `cpu-release-addr` property, identity-mapped at this point in boot.
/// `entry` must be a valid physical address of executable code.
pub unsafe fn write_release_addr(release_addr: u64, entry: u64) {
    // SAFETY: release_addr is a DTB-provided physical address, identity-mapped during boot
    let ptr = release_addr as *mut u64;
    // Write entry point to the CPU's release address (identity-mapped)
    core::ptr::write_volatile(ptr, entry);
    // Ensure the write is observable by other cores before SEV
    asm!("dsb sy", options(nomem, nostack));
}
