# Why GIC and Timer Live in the Kernel

Lockjaw's design rule is **all drivers in userspace** — except the interrupt controller (GIC) and the timer. This doc explains why these two are the exceptions.

## The interrupt controller must be in the kernel

The GIC (Generic Interrupt Controller) is the hardware that tells the CPU "an interrupt happened, here's which one." Every interrupt on the system flows through it. The kernel needs to:

1. **Acknowledge interrupts immediately.** When an IRQ fires, the CPU traps into the kernel's exception vector. The very first thing the handler must do is read the GIC's IAR register to find out which interrupt fired. If we had to IPC to a userspace driver to do this, we'd be taking an interrupt, context-switching to userspace, doing an MMIO read, context-switching back, and only then knowing what happened. That latency is unacceptable — and during that time, the interrupt is still pending.

2. **Control interrupt masking and priority.** The kernel must decide which interrupts are enabled, at what priority, and when to mask/unmask them. These decisions are tightly coupled to scheduling (e.g., disabling interrupts during a critical section, re-enabling after a context switch). Putting this in userspace would mean the kernel can't control its own preemption.

3. **Route interrupts to the right place.** When a userspace driver registers for an IRQ (Phase 9), the kernel programs the GIC to enable that IRQ and then delivers it to the driver as an IPC notification. The GIC is the mechanism that makes userspace IRQ delivery possible — it can't itself be in userspace.

In seL4's terminology, the GIC is part of the kernel's "platform support" — it's not a driver in the OS-services sense, it's part of the trap/exception handling infrastructure.

## The timer must be in the kernel

The timer drives preemptive scheduling. When a thread's time slice expires, the timer fires an interrupt and the kernel picks the next thread to run. This must happen inside the kernel because:

1. **The scheduler depends on it.** The timer interrupt is what triggers `schedule()`. If the timer were in userspace, a misbehaving process could simply not run the timer driver, preventing itself from ever being preempted.

2. **It's one register write.** The AArch64 virtual timer is controlled by two system registers (`CNTV_TVAL_EL0` and `CNTV_CTL_EL0`). There's no complex driver logic — just "set countdown, enable, handle interrupt, repeat." Putting this in userspace would add IPC overhead for zero benefit.

## Everything else goes to userspace

The UART, block devices, network — these are all userspace drivers in Lockjaw. They receive hardware interrupts via IPC notifications from the kernel (the kernel acknowledges the GIC, then signals a Notification object that wakes the driver). They access device MMIO through mapped device memory frames granted via capabilities. The kernel never touches their hardware directly after boot.

The early-boot UART access in `src/arch/aarch64/pl011.rs` is a temporary bootstrap mechanism. In Phase 9, a standalone userspace UART server takes over, and the kernel stops accessing the UART entirely.
