//! Safe facade for typed handle-table operations.
//!
//! Each function does: handle lookup → type check → typed IPC/notification
//! operation → drop. The mutable object reference is created and destroyed
//! internally — safe callers never see `&mut EndpointObject` or
//! `&mut NotificationObject`.
//!
//! This follows the same pattern as [`CurrentThread`](crate::sched::current::CurrentThread):
//! narrow safe methods, no general-purpose mutable borrow escaping.

use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::ipc::endpoint;
use crate::ipc::notification;
use crate::ipc::reply;
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::current::CurrentThread;
use lockjaw_types::object::HandleKind;
use lockjaw_types::syscall::SyscallError;

/// Send a message on an endpoint. Blocks the sender if no receiver is waiting.
pub fn send(handle: u32, msg: [u64; 4]) -> Result<Result<(), endpoint::IpcError>, SyscallError> {
    let entry = CurrentThread::handle_table().lookup(
        handle,
        Rights::from_bits(crate::cap::rights::RIGHT_WRITE),
        ObjectType::Endpoint,
    )?;
    let caller_token = match entry.kind {
        HandleKind::Endpoint { caller_token } if caller_token != 0 => caller_token,
        HandleKind::Endpoint { .. } => return Err(SyscallError::INVALID_PARAMETER),
        _ => return Err(SyscallError::INVALID_PARAMETER),
    };
    // SAFETY: handle_lookup verified type == Endpoint; object lives in a
    // kernel-owned page; KernelMut is created and dropped within this call.
    // Blocking — pass raw pointer so no &mut T survives across block_current().
    let km = unsafe { KernelMut::<endpoint::EndpointObject>::from_paddr(PhysAddr::new(entry.object_paddr)) };
    Ok(endpoint::ipc_send(km.raw_ptr(), msg, caller_token))
}

/// Receive a message from an endpoint. Blocks if no sender/caller is queued.
pub fn receive(handle: u32) -> Result<Result<[u64; 4], endpoint::IpcError>, SyscallError> {
    let entry = CurrentThread::handle_table().lookup(
        handle,
        Rights::from_bits(crate::cap::rights::RIGHT_READ),
        ObjectType::Endpoint,
    )?;
    // Blocking — pass raw pointer so no &mut T survives across block_current().
    let km = unsafe { KernelMut::<endpoint::EndpointObject>::from_paddr(PhysAddr::new(entry.object_paddr)) };
    Ok(endpoint::ipc_receive(km.raw_ptr()))
}

/// Send a message and block for a reply (client call). The Reply object must
/// be Fresh; on return it is Fresh again, ready for the next call.
pub fn call(
    ep_handle: u32,
    reply_handle: u32,
    msg: [u64; 4],
) -> Result<Result<[u64; 4], endpoint::IpcError>, SyscallError> {
    let ht = CurrentThread::handle_table();
    let rw = Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE);
    let ep_entry = ht.lookup(ep_handle, rw, ObjectType::Endpoint)?;
    let reply_entry = ht.lookup(reply_handle, rw, ObjectType::Reply)?;
    let caller_token = match ep_entry.kind {
        HandleKind::Endpoint { caller_token } if caller_token != 0 => caller_token,
        HandleKind::Endpoint { .. } => return Err(SyscallError::INVALID_PARAMETER),
        _ => return Err(SyscallError::INVALID_PARAMETER),
    };
    // SAFETY: both types verified; endpoint and reply are distinct objects
    // on distinct pages; both KernelMuts scoped to this call.
    // Blocking — pass raw pointers so no &mut T survives across block_current().
    let ep_km = unsafe { KernelMut::<endpoint::EndpointObject>::from_paddr(PhysAddr::new(ep_entry.object_paddr)) };
    let reply_km = unsafe { KernelMut::<reply::ReplyObject>::from_paddr(PhysAddr::new(reply_entry.object_paddr)) };
    Ok(endpoint::ipc_call(ep_km.raw_ptr(), reply_km.raw_ptr(), msg, caller_token))
}

/// Non-blocking receive. Returns the IPC WouldBlock error if no sender is waiting.
pub fn recv_nb(handle: u32) -> Result<Result<[u64; 4], endpoint::IpcError>, SyscallError> {
    let entry = CurrentThread::handle_table().lookup(
        handle,
        Rights::from_bits(crate::cap::rights::RIGHT_READ),
        ObjectType::Endpoint,
    )?;
    // SAFETY: same as send — type verified, KernelMut scoped to this call.
    let mut km = unsafe { KernelMut::<endpoint::EndpointObject>::from_paddr(PhysAddr::new(entry.object_paddr)) };
    Ok(endpoint::ipc_receive_nb(km.get_mut()))
}

/// Signal a notification with a new timeline value.
pub fn signal_notification(
    handle: u32,
    new_value: u64,
) -> Result<Result<(), lockjaw_types::notification_state::NotificationError>, SyscallError> {
    let entry = CurrentThread::handle_table().lookup(
        handle,
        Rights::from_bits(crate::cap::rights::RIGHT_WRITE),
        ObjectType::Notification,
    )?;
    // SAFETY: type verified as Notification; KernelMut scoped to this call.
    let mut km = unsafe { KernelMut::<notification::NotificationObject>::from_paddr(PhysAddr::new(entry.object_paddr)) };
    Ok(notification::notification_signal(km.get_mut(), new_value))
}

/// Wait on a notification until the timeline value reaches the threshold.
/// Returns immediately if the counter is already >= threshold, otherwise blocks.
pub fn wait_notification(
    handle: u32,
    threshold: u64,
) -> Result<Result<u64, lockjaw_types::notification_state::NotificationError>, SyscallError> {
    let entry = CurrentThread::handle_table().lookup(
        handle,
        Rights::from_bits(crate::cap::rights::RIGHT_READ),
        ObjectType::Notification,
    )?;
    // Blocking — pass raw pointer so no &mut T survives across block_current().
    let km = unsafe { KernelMut::<notification::NotificationObject>::from_paddr(PhysAddr::new(entry.object_paddr)) };
    Ok(notification::notification_wait(km.raw_ptr(), threshold))
}
