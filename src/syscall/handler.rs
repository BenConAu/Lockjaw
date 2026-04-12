use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;
use crate::cap::handle_table;
use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::ipc::endpoint;
use crate::mm::addr::PhysAddr;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;

/// Syscall numbers.
const SYS_DEBUG_PUTC: u64 = 0;
const SYS_YIELD: u64 = 1;
const SYS_SEND: u64 = 2;
const SYS_RECEIVE: u64 = 3;
const SYS_CALL: u64 = 4;
const SYS_REPLY: u64 = 5;
const SYS_ALLOC_PAGES: u64 = 6;

/// Dispatch a syscall from userspace.
/// Called from handle_exception_sync_lower when EC = 0x15 (SVC from AArch64).
///
/// Convention: syscall number in x8, arguments in x0-x5, return in x0.
/// Returns 0 on success, u64::MAX on error.
pub fn handle_syscall(ctx: &mut ExceptionContext) {
    let syscall_num = ctx.gpr[8]; // x8

    ctx.gpr[0] = match syscall_num {
        SYS_DEBUG_PUTC => sys_debug_putc(ctx.gpr[0]),
        SYS_YIELD => sys_yield(),
        SYS_SEND => sys_send(ctx),
        SYS_RECEIVE => sys_receive(ctx),
        SYS_CALL => sys_call(ctx),
        SYS_REPLY => sys_reply(ctx),
        SYS_ALLOC_PAGES => sys_alloc_pages(ctx),
        _ => {
            crate::kprintln!("Unknown syscall {}", syscall_num);
            u64::MAX
        }
    };
}

fn sys_debug_putc(char_val: u64) -> u64 {
    let uart = Uart::new();
    uart.putc(char_val as u8);
    0
}

fn sys_yield() -> u64 {
    unsafe { scheduler::tick(); }
    0
}

/// sys_send(handle, msg0, msg1, msg2, msg3) — send a message on an endpoint.
/// x0 = endpoint handle, x1-x4 = message registers.
fn sys_send(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        // Look up the endpoint handle
        let entry = match handle_table::handle_lookup(ht_paddr, handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE)) {
            Ok(e) => e,
            Err(_) => return u64::MAX,
        };

        if entry.obj_type != ObjectType::Endpoint {
            return u64::MAX;
        }

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_send(ep_paddr, msg, tcb_paddr) {
            Ok(()) => 0,
            Err(_) => u64::MAX,
        }
    }
}

/// sys_receive(handle) — receive a message from an endpoint.
/// x0 = endpoint handle. Message returned in x0-x3.
fn sys_receive(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;

    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        let entry = match handle_table::handle_lookup(ht_paddr, handle, Rights::from_bits(crate::cap::rights::RIGHT_READ)) {
            Ok(e) => e,
            Err(_) => return u64::MAX,
        };

        if entry.obj_type != ObjectType::Endpoint {
            return u64::MAX;
        }

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_receive(ep_paddr, tcb_paddr) {
            Ok(msg) => {
                ctx.gpr[0] = msg[0];
                ctx.gpr[1] = msg[1];
                ctx.gpr[2] = msg[2];
                ctx.gpr[3] = msg[3];
                return msg[0];
            }
            Err(_) => return u64::MAX,
        }
    }
}

/// sys_call(handle, msg0, msg1, msg2, msg3) — send message and block for reply.
/// Combines send + receive in one syscall. Returns the reply in x0-x3.
fn sys_call(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        let entry = match handle_table::handle_lookup(ht_paddr, handle, Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE)) {
            Ok(e) => e,
            Err(_) => return u64::MAX,
        };

        if entry.obj_type != ObjectType::Endpoint {
            return u64::MAX;
        }

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_call(ep_paddr, msg, tcb_paddr) {
            Ok(reply) => {
                ctx.gpr[0] = reply[0];
                ctx.gpr[1] = reply[1];
                ctx.gpr[2] = reply[2];
                ctx.gpr[3] = reply[3];
                return reply[0];
            }
            Err(_) => return u64::MAX,
        }
    }
}

/// sys_reply(msg0, msg1, msg2, msg3) — reply to the last caller on handle 0.
/// x0 = handle, x1-x4 = reply message.
fn sys_reply(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let reply_msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        let entry = match handle_table::handle_lookup(ht_paddr, handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE)) {
            Ok(e) => e,
            Err(_) => return u64::MAX,
        };

        if entry.obj_type != ObjectType::Endpoint {
            return u64::MAX;
        }

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_reply(ep_paddr, reply_msg) {
            Ok(()) => 0,
            Err(_) => u64::MAX,
        }
    }
}

/// sys_alloc_pages(count) — allocate physical pages.
/// x0 = number of pages to allocate.
/// Returns a PageSet ID on success, u64::MAX on failure.
fn sys_alloc_pages(ctx: &mut ExceptionContext) -> u64 {
    let count = ctx.gpr[0] as usize;

    match crate::cap::pageset_table::alloc_pages(count) {
        Some(id) => id,
        None => u64::MAX,
    }
}
