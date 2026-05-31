# Non-Virtualizable Hardware and the Device Manager

> *Some resources are one. The kernel cannot hand out copies of one.*

Lockjaw's architecture leans on capability-style direct handles
wherever possible: a process holds a handle to a `PageSet`, a
`Notification`, an `Endpoint`, and operates on it without anyone
else in the loop. That model works because those resources
*virtualize* — the kernel can make one CPU look like many threads,
one address space look like many mappings, one endpoint object
support many sender identities. Each holder operates on its own
slice of a fundamentally divisible resource.

Hardware is not always like that. Some hardware resources are
**non-virtualizable**: there is exactly one of them in the SoC, and
the side effects of operating on it touch state that other clients
also depend on. A driver cannot have its "own" PLL the way it has
its own page. This chapter pins down the architectural consequence:
**non-virtualizable hardware resources are arbitrated through the
device manager, and clients never receive a direct handle to the
provider.** The clock tree (M0a–M0c) is the first instance; the
pattern will recur.

## The requirement

Two drivers must not be able to independently configure the same
non-virtualizable hardware resource into incompatible states
without an arbiter rejecting the conflict.

The PLL example: a SoC has a finite number of PLLs. Each PLL feeds
multiple downstream gates (UART, SDHCI, GPIO, ...). Setting the
parent PLL rate affects every consumer of every gate beneath it.
If two drivers each held a "clock handle" and could independently
call `set_rate`, the second call would silently retune everyone
under the first driver's PLL.

The MAIR example (M6, future): the AArch64 MMU has 8 attribute
slots in `MAIR_EL1`. Each slot's contents apply to every PTE
referencing it. Two drivers cannot independently "own" slot 4 — a
write by one is visible to all PTEs.

The IRQ-line example (already in place via device-manager today):
each `INTID` is wired to one peripheral. Two drivers binding the
same IRQ would each receive notifications for hardware they don't
own.

These are not edge cases. They are the *defining property* of
non-virtualizable hardware: state changes have shared, externally-
visible side effects.

## What follows from the requirement

A single entity must mediate every operation that mutates the
shared state. That entity must:

1. Know who is allowed to operate on each leaf of the resource
   (the binding from caller identity to leaf identity).
2. Be the only path through which operations can reach the
   provider — otherwise clients can bypass it.
3. Be unforgeable as the operator of the provider — otherwise
   clients can impersonate the broker.

In Lockjaw, **the device manager is that entity** for hardware
resources, because the device manager already owns the analogous
arbitration for MMIO regions and IRQ bindings (one driver per
device, enforced via `CMD_CLAIM_DEVICE` / `CMD_PROBE_DEVICE` /
`CMD_CLAIM_BY_ADDR`). Extending it to clocks (and later to other
shared hardware) keeps arbitration in one place rather than
scattering policy across drivers.

The provider — `cprman-driver` for clocks — owns the *mechanism*
(the MMIO writes, the divider math, the leaf-specific protocol).
The device manager owns the *policy* (who is allowed to operate
on which leaf, and forwarding the operation).

## What does NOT follow from the requirement

The requirement does not pick the IPC shape. Two shapes can satisfy
"a single arbiter mediates every operation":

- **Proxy through the arbiter.** Drivers call the arbiter; the
  arbiter calls the provider; the arbiter routes the reply back.
  Three IPC hops per operation. Drivers never hold a handle to
  the provider.
- **Per-binding endpoints.** The arbiter creates a fresh endpoint
  per (client, leaf) binding; drivers receive their endpoint and
  call the provider directly. The provider serves N endpoints,
  each baked to a specific leaf. One IPC hop per operation.

Both satisfy the requirement. Lockjaw chooses **proxy through the
device manager** because:

- The device manager already speaks the IPC vocabulary that brokers
  this kind of identity (per-caller-token tables, the same shape
  as `BlockEngine`'s `BufferTracker`).
- The arbiter holding the only handle to the provider makes the
  trust boundary trivial: the provider's only legitimate caller is
  the device manager, which is in the boot trust chain.

The choice is **not** justified by:

- *Operation count or latency.* Three-hop IPC has a real cost. We
  accept it. If a future workload makes clock ops latency-critical
  (sub-millisecond budgets, hot path), the right answer is to
  move the call sites off the hot path, not to switch IPC shapes.
- *Ease of implementation.* Per-binding endpoints are a different
  set of complexity (multiplexing receives, a finite endpoint
  budget per provider), not a higher one. The reason to prefer
  proxy is the trust-boundary clarity above, not that proxy is
  shorter to write.

If the requirement changes — for example, if Lockjaw needs to
support clock providers that themselves multiplex many independent
client streams (a clock chip with N independent PLLs that don't
share parent state, where per-binding endpoints could let drivers
talk to separate PLLs without serializing through the device
manager) — revisit. The proxy shape will start costing real
parallelism then; per-binding endpoints will start paying for
themselves.

## The IPC shape

For clocks (the M0c instance):

```
driver ─────CLOCK_OP_SET_RATE(handle_id, hz)────────►  device-manager
                                                            │
                                                  per-driver handle table
                                                  (caller_token, handle_id)
                                                       → (controller, clock_id)
                                                            │
                                                   ┌────────┴───────┐
                                                   │                │
              ◄───reply (status, actual_hz)─── device-manager       │
                                                   │                │
                                                   ▼                │
                                            cprman.SET_RATE(clock_id, hz)
                                                   │                │
                                                   ▼                │
                                          ┌────────────────┐        │
                                          │ cprman-driver  │        │
                                          │ (sole caller:  │        │
                                          │  device-manager)│       │
                                          └────────────────┘        │
                                                   │                │
                                       reply (status, actual_hz) ──┘
```

Properties this shape has:

- Drivers never hold a handle to `cprman-driver`. They hold only
  a handle to `device-manager` and an opaque `handle_id: u32` that
  device-manager allocated for them.
- The `(caller_token, handle_id)` → `(controller_phandle, clock_id)`
  table lives entirely in device-manager. The caller_token comes
  from the kernel (always-mint per-export — see chapter 02), so two
  drivers calling device-manager arrive with distinct identities,
  even if device-manager re-exported endpoints to them.
- `cprman-driver` has exactly one legitimate caller
  (device-manager). cprman dispatches on the message body's
  `clock_id` and trusts it because the only other entity that
  could have crafted the message is device-manager, which owns the
  binding table.
- A driver cannot pivot its `handle_id` to another clock_id: the
  binding is fixed in device-manager's table at `CMD_GET_CLOCK_HANDLE`
  time and the lookup is scoped by the driver's caller_token, so
  guessing another driver's `handle_id` doesn't help either.

### The provider registry

device-manager builds a small provider registry at startup by
scanning its parsed DTB. For each clock-provider compatible string
it knows about (today: `brcm,bcm2711-cprman`), it captures the
device's DTB phandle into the registry. `CMD_GET_CLOCK_HANDLE`
checks the requested `controller_phandle` against this registry
and returns `CLOCK_ERR_NO_PROVIDER` for any phandle that isn't
registered.

This is what makes "the binding lives in device-manager" load-
bearing: without the registry, a malicious or buggy client could
ask for any phandle and receive a handle that forwards to cprman
unconditionally. The registry is what closes the validation gate.

On QEMU virt there is no `bcm2711-cprman` node, so the registry is
empty and every clock request returns `NoProvider`. This is the
correct behaviour for the platform — Lockjaw on QEMU has no clock
provider — and it is what the integration test asserts. The
SET_RATE / GET_RATE / ENABLE / DISABLE forwarding paths are
unreachable on QEMU because no valid handle can be obtained; they
will be exercised on Pi 4B once an actual driver (M1's
`emmc2-driver`) acquires a real handle from a real
`emmc2.clocks = <&cprman 51>` reference.

Future multi-provider support (e.g., a board with both BCM2711
CPRMAN and an I2C clock chip) extends the registry: each provider
gets its own row, and `CLOCK_OP_*` forwarding routes by
`binding.controller_phandle` to the matching provider's endpoint
rather than the single `cprman_client` handle today's code uses.
The shape of the rest of the model — drivers never holding direct
provider handles, identity table in device-manager, opaque
handle_id from the driver's perspective — does not change.

## What this chapter is for

If you are about to add a new hardware-provider driver and find
yourself wondering whether to give clients a direct handle or
broker through device-manager, the procedure is:

1. Ask whether the resource is virtualizable. If yes (every
   client gets its own slice with no cross-client side effects),
   direct handle is fine. PageSets, endpoints, notifications fall
   here.
2. If not virtualizable, broker through device-manager. The
   provider's only legitimate caller is device-manager; clients
   get an opaque handle_id that device-manager translates.
3. If you find yourself reaching for "but proxy is too slow" —
   recheck the operation count. Setup-time operations (allocating
   a clock binding, reserving a MAIR slot, claiming an IRQ) are
   typically once per driver lifetime; the proxy cost is in the
   noise. If they truly are on a hot path, escalate the design
   instead of shortcutting the trust boundary.

The pattern that brought us here was: I almost picked the proxy
shape for ease/performance reasons, which would have been the
right answer for the wrong reason. Ben caught it. The architectural
basis is non-virtualizability — and *that* is what should drive
the decision when the next non-virtualizable hardware resource
shows up.
