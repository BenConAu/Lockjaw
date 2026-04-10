use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;

/// Syscall numbers.
const SYS_DEBUG_PUTC: u64 = 0;

/// Dispatch a syscall from userspace.
/// Called from handle_exception_sync_lower when EC = 0x15 (SVC from AArch64).
///
/// Convention: syscall number in x8, arguments in x0-x5, return in x0.
/// Returns 0 on success, u64::MAX on error.
pub fn handle_syscall(ctx: &mut ExceptionContext) {
    let syscall_num = ctx.gpr[8]; // x8
    let arg0 = ctx.gpr[0];        // x0

    ctx.gpr[0] = match syscall_num {
        SYS_DEBUG_PUTC => sys_debug_putc(arg0),
        _ => {
            crate::kprintln!("Unknown syscall {}", syscall_num);
            u64::MAX
        }
    };
}

/// sys_debug_putc(char): print a single character on behalf of userspace.
/// This is a temporary bootstrap syscall — in Phase 9, UART output moves
/// to a userspace driver and this syscall is removed.
fn sys_debug_putc(char_val: u64) -> u64 {
    let uart = Uart::new();
    uart.putc(char_val as u8);
    0
}
