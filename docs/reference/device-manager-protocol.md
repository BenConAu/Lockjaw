# Device Manager Protocol

The device-manager is the userspace arbiter for non-virtualizable
hardware — MMIO ranges, IRQs, and clock providers all funnel through
it. Each driver claims its device(s) by hash or address, and the
device-manager replies with an exported PageSet handle for the MMIO
page plus the IRQ number plus a packed clock-reference word.

This doc is the canonical reference for the wire protocol. The *why*
of having a userspace arbiter at all is in
[`../architecture/03-non-virtualizable-hardware.md`](../architecture/03-non-virtualizable-hardware.md).
The userlib wrapper that drivers actually call is `claim_typed` in
`user/lockjaw-userlib/src/devmgr.rs:125`. For the clock subsystem
that hangs off CMD_GET_CLOCK_HANDLE, see
[`clock-subsystem.md`](clock-subsystem.md).

## The 4 device CMDs

All CMD constants live at `lockjaw-types/src/device.rs:138-193`.
Messages are the standard 4 words; reply convention is `(status,
...)` in `x1-x4` per [`ipc.md`](ipc.md).

### `CMD_CLAIM_DEVICE = 1`

Claim the **first unclaimed** device matching a compatible hash.

```text
Request:  [CMD_CLAIM_DEVICE, compatible_hash, 0, 0]
Response: [status, exported_handle, intid, packed_clock_ref]
```

`compatible_hash` is the FNV-1a hash of the DTB `compatible` string
(e.g., `b"arm,pl011"`). Pre-computed constants live at
`lockjaw-types/src/device.rs:23+`: `PL011_HASH`, `FW_CFG_HASH`,
`VIRTIO_MMIO_HASH`, `BCM2711_CPRMAN_HASH`, `BCM2711_EMMC2_HASH`.
New devices get a constant added there, computed via the const-fn
`compatible_hash(s: &[u8])`.

| Status | Meaning |
|---|---|
| `CLAIM_OK = 0` | Claim succeeded; `exported_handle` is a PageSet handle for the MMIO page. |
| `CLAIM_ERR = 1` | No matching unclaimed device, or internal failure. |

When `CLAIM_OK`:
- `exported_handle` is in the caller's handle table; it's a PageSet
  whose pages are the device's MMIO. Handle 0 is a valid index —
  check `status`, not the handle value.
- `intid` is the device's IRQ number (0 if the node has no IRQ).
- `packed_clock_ref` carries the device's first DTB `clocks = <&phandle id>`
  reference, packed via `pack_clock_ref(controller_phandle, clock_id)`
  (`device.rs:209`). 0 means the node had no `clocks` property.
  Drivers with a clock dep call `CMD_GET_CLOCK_HANDLE` (see
  [`clock-subsystem.md`](clock-subsystem.md)) using these
  unpacked fields directly — no separate query needed.

This is the simple-case claim. For devices where ordering or
TOCTOU matters, use `CMD_PROBE_DEVICE` + `CMD_CLAIM_BY_ADDR`.

### `CMD_PROBE_DEVICE = 2`

Probe a device by absolute index among **all** devices matching a
compatible hash (including already-claimed ones). The index is over
the full DTB-derived list, so it's stable regardless of concurrent
claims.

```text
Request:  [CMD_PROBE_DEVICE, compatible_hash, index, 0]
Response: [status, mmio_addr, intid, device_id]
```

For unclaimed devices the device-manager temporarily maps the MMIO
page, reads the device_id register, unmaps, and returns the result.
Magic-byte validation happens internally; on bad magic the response
is `PROBE_ERR`.

| Status | Meaning |
|---|---|
| `PROBE_OK = 0` | Device found; `device_id` valid. |
| `PROBE_END = 1` | No device at this index (caller walked past the end). |
| `PROBE_CLAIMED = 2` | Device exists but already claimed by another driver. |
| `PROBE_ERR = 3` | Internal failure (map/register/bad magic). |

`mmio_addr`, `intid`, and `device_id` are only meaningful on
`PROBE_OK`. Typical use: virtio-blk walks `(VIRTIO_MMIO_HASH, 0..N)`,
filters on `device_id == DEVICE_ID_BLOCK`, then `CMD_CLAIM_BY_ADDR`s
the matching slot.

### `CMD_CLAIM_BY_ADDR = 3`

Claim a device by its exact MMIO physical address. The TOCTOU-safe
variant of `CMD_CLAIM_DEVICE`: the driver already knows the address
from probe, no skip_count, no race.

```text
Request:  [CMD_CLAIM_BY_ADDR, mmio_addr, 0, 0]
Response: [status, exported_handle, intid, packed_clock_ref]
```

Status codes + response layout identical to `CMD_CLAIM_DEVICE`.

### `CMD_RELEASE_BY_ADDR = 4`

Release a previously claimed device, clearing the device-manager's
`claimed` bit so the same `mmio_addr` becomes claimable again.

```text
Request:  [CMD_RELEASE_BY_ADDR, mmio_addr, 0, 0]
Response: [status, 0, 0, 0]
```

`status = CLAIM_OK` if the address matched a claimed device whose
`claim_token` matches the caller's IPC token (verified by the
identity model — see "Ownership" below); `CLAIM_ERR` otherwise.

**Caller obligation:** the exported MMIO pageset handle MUST be
closed BEFORE issuing the release. The device-manager has no
kernel-side reference tracking, so it cannot itself verify the
driver has actually relinquished the mapping. If a release fires
while the original claimant still holds the exported handle, a
second driver can reclaim and map the same device — a real race.
`claim_typed` enforces the ordering (drops the guard, then
releases); other callers must follow the same discipline. The
verified-release follow-up is tracked in
[`../tracking/tech-debt.md`](../tracking/tech-debt.md).

## Ownership: who owns a claim?

The device-manager records each claim's `claim_token` — the IPC
identity token of the caller that succeeded. Subsequent
`CMD_RELEASE_BY_ADDR` requests are accepted only if the caller's
identity matches. Different drivers cannot release each other's
claims; a malicious caller cannot steal a claim by sending its own
release. The token model is detailed in
[`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md);
the per-call identity is exposed to servers via
`sys_query_caller_token` (#26 in [`syscalls.md`](syscalls.md)).

## Clock-reference encoding

`pack_clock_ref(controller_phandle, clock_id)` packs two `u32`s
into one `u64`: phandle in the high 32 bits, id in the low. Zero
unpacks to "no provider, no clock id" — the inverse `unpack_clock_ref`
returns `None` for zero, `Some((phandle, id))` otherwise (`device.rs:216`).

The encoding exists so a driver can pull the clock reference out of
the claim reply and pass it straight to `CMD_GET_CLOCK_HANDLE`
without a separate `CMD_QUERY_CLOCKS` round-trip. The
[`clock-subsystem.md`](clock-subsystem.md) doc covers the rest of
the clock IPC.

## The userlib wrapper

Drivers don't type `sys_call` against `CMD_CLAIM_*` directly. The
canonical entry point is `claim_typed::<T>(devmgr_ep, reply_obj, mmio_addr)`
at `user/lockjaw-userlib/src/devmgr.rs:125`:

```rust
pub fn claim_typed<T: 'static>(
    devmgr_ep: EndpointHandle,
    reply_obj: ReplyHandle,
    mmio_addr: u64,
) -> Result<ClaimedDevice<T>, ClaimError>;
```

It does these things in order:

1. **Layout check** (pre-IPC): rejects with `LayoutMisaligned` /
   `LayoutTooLarge` if `T`'s alignment or page-offset claim is
   impossible. A wrong `T` for this address is a programming bug,
   not a runtime claim/release cycle.
2. **`CMD_CLAIM_BY_ADDR`** via `sys_call_ret4`.
3. **`PageSetGuard`** wraps the returned handle so a subsequent
   VMEM/map failure closes the pageset.
4. **`sys_map_pages`** the MMIO into the driver's VA range.
5. On failure between claim-success and map-success: close the
   pageset (drop the guard), then `CMD_RELEASE_BY_ADDR` so the
   address is reclaimable. The plain claim path leaks the claim
   on transient VMEM failure — `claim_typed` is the closed-loop
   version.
6. Return a `ClaimedDevice<T>` with the typed `MappedRegs<T>`,
   the IRQ intid, and (if present) the `ClockRef`.

`claim_typed` is what drivers actually use; the raw CMD codes are
documented here for completeness and for anyone debugging the wire.

## Device-manager source map

| File | Role |
|---|---|
| `user/device-manager/src/main.rs` | The server. CMD dispatch + per-CMD handlers. |
| `lockjaw-types/src/device.rs` | Constants (`PL011_HASH`, etc.), `CMD_*` codes, status codes, `pack_clock_ref`. |
| `lockjaw-types/src/fdt.rs` | DTB walker; populates the device list the manager arbitrates. |
| `lockjaw-types/src/clock/` | Clock-subsystem IPC (`CMD_GET_CLOCK_HANDLE` and the per-clock ops). |
| `user/lockjaw-userlib/src/devmgr.rs` | Driver-facing wrappers (`claim_typed`, `ClaimError`, `ClaimedDevice<T>`). |

For any new device type: add a `compatible_hash` constant in
`lockjaw-types/src/device.rs`, then drivers can claim by that hash.
The device-manager itself doesn't need code changes for new
hash-matched devices — the matching is data-driven through the
hash list.
