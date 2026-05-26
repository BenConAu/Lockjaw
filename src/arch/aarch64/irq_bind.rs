use core::cell::UnsafeCell;
use lockjaw_types::addr::KernelVa;

/// Maximum number of IRQ-to-Notification bindings.
///
/// Must cover QEMU virt's UART (INTID 33-40) and virtio-mmio
/// (INTID 48-79) AND Pi 4B's higher-numbered SPIs: PL011 = 153,
/// emmc2 = 158. The pre-B4.3 ceiling of 96 silently rejected Pi
/// bindings — bind() returned false on Pi IRQ INTIDs but the
/// polling-only driver stack masked the failure. Raising to 256
/// covers every SPI in the BCM2711's IRQ space (the GICv2
/// distributor on Pi 4B reports 256 IRQ lines per the boot log)
/// and unblocks the m7-irq-experiment merge that wires emmc2 to
/// IRQ-driven completion. Cost: Option<u64> × 256 = 4 KiB in BSS,
/// vs the prior 1.5 KiB; well within static budget.
const MAX_BINDINGS: usize = 256;

// ---------------------------------------------------------------------------
// IrqBindings singleton
// ---------------------------------------------------------------------------

/// Static IRQ binding table. Maps hardware INTID → Notification KVA.
/// Notification objects live in the KVM pool (see kernel-vmem-roadmap.md),
/// so the table stores `KernelVa` and the IRQ handler reaches the
/// NotificationObject via `from_kva`. Wraps the mutable array in
/// `UnsafeCell` so the `unsafe impl Sync` safety argument lives in one
/// place rather than at every call site.
struct IrqBindings(UnsafeCell<[Option<KernelVa>; MAX_BINDINGS]>);

/// SAFETY: single-core kernel. IRQ handler reads this table; syscall
/// handler writes it. Both run with IRQs masked at EL1, so no concurrent
/// access is possible.
unsafe impl Sync for IrqBindings {}

static BINDINGS: IrqBindings = IrqBindings(UnsafeCell::new([None; MAX_BINDINGS]));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Bind a hardware INTID to a Notification object.
/// When the IRQ fires, the kernel will signal this notification.
/// Returns false if the INTID is out of range or already bound.
/// Reserved INTIDs that cannot be bound by userspace.
/// INTID 0 = kernel reschedule SGI (cross-core wakeup).
const RESERVED_INTID_SGI_RESCHED: u32 = 0;

pub fn bind(intid: u32, notification_kva: KernelVa) -> bool {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS || intid == RESERVED_INTID_SGI_RESCHED {
        return false;
    }
    // SAFETY: single-core, IRQs masked — exclusive access.
    unsafe {
        let table = &mut *BINDINGS.0.get();
        if table[idx].is_some() {
            return false; // already bound
        }
        table[idx] = Some(notification_kva);
    }
    true
}

/// Look up the Notification KVA bound to a given INTID.
/// Returns None if the INTID is unbound or out of range.
pub fn lookup(intid: u32) -> Option<KernelVa> {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return None;
    }
    // SAFETY: single-core, IRQs masked — read-only access.
    unsafe { (*BINDINGS.0.get())[idx] }
}
