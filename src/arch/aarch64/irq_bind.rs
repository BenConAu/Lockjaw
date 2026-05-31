use core::cell::UnsafeCell;
use lockjaw_types::addr::KernelVa;

/// Maximum number of IRQ-to-Notification bindings.
/// Must cover UART (INTID 33-40), virtio-mmio (INTID 48-79), and
/// BCM2711 SPI peripherals which can extend to INTID ~190 (emmc2 is
/// SPI 126 → INTID 158). 256 leaves comfortable headroom for the
/// rest of the BCM2711 SPI range. Known limitation: static table,
/// see docs/tracking/tech-debt.md.
///
/// (Constant was raised from 96 to 256 in commit ba13c18 as B4.3 of
/// the post-c1 fix plan, ahead of this IRQ-framework graft, so the
/// bump is isolable from the structural Binding-struct changes that
/// land alongside it here.)
const MAX_BINDINGS: usize = 256;

/// Per-binding state. Tracks the notification object the IRQ signals,
/// whether the IRQ source is level-triggered, AND the process that
/// owns the binding. Level-triggered sources require the kernel to
/// disable the IRQ in the GIC between signaling userspace and
/// userspace clearing the source — otherwise the line stays asserted
/// and the IRQ refires immediately after EOIR. Edge-triggered sources
/// don't need this because the GIC only fires once per rising edge.
///
/// The owner field gates `sys_unmask_irq` so that only the process
/// that bound a given INTID can re-enable it in the GIC distributor.
/// Without this, any process holding any binding could flip the GIC
/// enable bit for an IRQ bound to a different driver — a cross-
/// process confused-deputy on per-driver device state. The binding
/// owner is recorded once at `bind()` time from the caller's
/// ProcessObject KVA (a kernel-pool address, unforgeable from
/// userspace).
#[derive(Copy, Clone)]
struct Binding {
    /// Notification object KVA. Kernel signals this on IRQ.
    notif_kva: KernelVa,
    /// ProcessObject KVA of the process that called `sys_bind_irq`.
    /// `sys_unmask_irq` rejects calls whose current process KVA
    /// does not equal this — see crate doc comment above.
    owner_process_kva: KernelVa,
    /// Source-side trigger semantics. `true` ⇒ kernel must
    /// `gic::disable_spi(intid)` after signaling, userspace must
    /// later call `sys_unmask_irq(intid)` to re-enable. `false`
    /// (edge) ⇒ no kernel-side mask needed; the GIC's edge
    /// semantics handle re-arming naturally.
    level_triggered: bool,
}

// ---------------------------------------------------------------------------
// IrqBindings singleton
// ---------------------------------------------------------------------------

/// Static IRQ binding table. Maps hardware INTID → Binding.
/// Notification objects live in the KVM pool (see kernel-vmem-roadmap.md),
/// so the table stores `KernelVa` and the IRQ handler reaches the
/// NotificationObject via `from_kva`. Wraps the mutable array in
/// `UnsafeCell` so the `unsafe impl Sync` safety argument lives in one
/// place rather than at every call site.
struct IrqBindings(UnsafeCell<[Option<Binding>; MAX_BINDINGS]>);

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

pub fn bind(
    intid: u32,
    notification_kva: KernelVa,
    owner_process_kva: KernelVa,
    level_triggered: bool,
) -> bool {
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
        table[idx] = Some(Binding {
            notif_kva: notification_kva,
            owner_process_kva,
            level_triggered,
        });
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
    unsafe { (*BINDINGS.0.get())[idx].map(|b| b.notif_kva) }
}

/// Whether the bound IRQ source is level-triggered. Returns
/// `false` if the intid is unbound, out of range, or edge.
///
/// Used by `irq_dispatch` to decide whether to mask the IRQ in the
/// GIC after signaling userspace.
pub fn is_level_triggered(intid: u32) -> bool {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return false;
    }
    // SAFETY: single-core, IRQs masked — read-only access.
    unsafe {
        (*BINDINGS.0.get())[idx]
            .map(|b| b.level_triggered)
            .unwrap_or(false)
    }
}

/// Whether the given process KVA owns the binding for this intid.
/// Returns `false` if the intid is unbound, out of range, or owned
/// by a different process. Used by `sys_unmask_irq` to reject
/// cross-process GIC-enable attempts (see Binding doc comment).
pub fn is_owner(intid: u32, process_kva: KernelVa) -> bool {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return false;
    }
    // SAFETY: single-core, IRQs masked — read-only access.
    unsafe {
        (*BINDINGS.0.get())[idx]
            .map(|b| b.owner_process_kva.as_u64() == process_kva.as_u64())
            .unwrap_or(false)
    }
}
