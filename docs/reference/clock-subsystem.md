# Clock Subsystem

The clock subsystem is how a driver acquires a handle to one
hardware clock and operates on it (set rate / get rate / enable /
disable). It rides on top of the device-manager protocol but lives
in its own opcode range (0x1000+) so it cannot collide with
hardware-arbitration commands.

This doc is the reference for the IPC layer. Why the architecture
splits clock providers from device-manager (each clock controller
is its own driver, devmgr proxies) is in
[`../architecture/03-non-virtualizable-hardware.md`](../architecture/03-non-virtualizable-hardware.md).
For the device-manager protocol the clock CMD rides on, see
[`device-manager-protocol.md`](device-manager-protocol.md).

## The two-leg model

A clock op crosses *two* IPC boundaries. The driver never talks to
the clock provider directly:

```text
                  CLOCK_OP_*, handle_id, arg
   Driver  ----------------------------------------->  Device-manager
        <-----------------------------------------
                  status, value

                                       CLOCK_OP_*, clock_id, arg
                    Device-manager  ------------------------------->  Provider
                                    <-------------------------------
                                       status, value
```

The driver's first leg carries an opaque `handle_id`; the
device-manager looks `(caller_token, handle_id)` up in its binding
table and rewrites the message to `(CLOCK_OP_*, clock_id, arg, 0)`
on the provider leg. The provider (e.g., cprman-driver on Pi 4B)
trusts that body because device-manager is its only legitimate
caller — no driver holds a handle to the provider directly.

Reply layout on both legs is the same:

```text
msg = [status, value, 0, 0]
```

`status` is one of CLOCK_OK / CLOCK_ERR_* (constants at
`lockjaw-types/src/clock/mod.rs:67-80`); `value` is op-specific —
the actual rate for SET_RATE/GET_RATE, 0 for ENABLE/DISABLE, the
allocated `handle_id` for the acquire path.

## Acquire — `CMD_GET_CLOCK_HANDLE`

Driver-only command; the provider does not see this.

```text
Request:  [CMD_GET_CLOCK_HANDLE = 0x1000, controller_phandle, clock_id, 0]
Response: [status, handle_id, 0, 0]
```

`controller_phandle` and `clock_id` come from the driver's first
DTB `clocks = <&phandle id>` reference — drivers don't query this
separately; the device-manager packs it into the claim reply (see
`pack_clock_ref` in [`device-manager-protocol.md`](device-manager-protocol.md)).
The driver unpacks it from the claim reply and passes the two
halves here.

On success, `handle_id` is an opaque `u32` the device-manager
allocated in its `ClockHandleTable` (cap = 32, see below). The
driver passes this opaque value to subsequent `CLOCK_OP_*` calls.

## Operate — the four CLOCK_OP_* opcodes

| Opcode | Value | Arg | Reply.value |
|---|---|---|---|
| `CLOCK_OP_SET_RATE` | `0x1001` | target Hz | actual Hz achieved |
| `CLOCK_OP_GET_RATE` | `0x1002` | (unused) | current Hz |
| `CLOCK_OP_ENABLE`   | `0x1003` | (unused) | 0 |
| `CLOCK_OP_DISABLE`  | `0x1004` | (unused) | 0 |

All four use the same wire shape: driver sends
`[opcode, handle_id, arg, 0]` to devmgr; devmgr rewrites and forwards
`[opcode, clock_id, arg, 0]` to the provider; provider replies
`[status, value, 0, 0]` which devmgr forwards back unchanged.

The opcodes live at `0x1001..0x1004` deliberately —
device-manager's hardware-arbitration CMDs are `1..4`, and the
driver→devmgr leg uses message word 0 for both spaces. A `1..4`
collision would silently misroute `CLOCK_OP_SET_RATE` to
`CMD_CLAIM_DEVICE` and corrupt state. The `0x1000+` bias makes the
two opcode spaces disjoint by construction.

## Status codes

All at `lockjaw-types/src/clock/mod.rs:67-80`:

| Code | Value | Source | Meaning |
|---|---|---|---|
| `CLOCK_OK`                  | 0 | both legs | success |
| `CLOCK_ERR_NOT_SUPPORTED`   | 1 | provider | clock_id not modeled |
| `CLOCK_ERR_OUT_OF_RANGE`    | 2 | provider | rate outside divider range |
| `CLOCK_ERR_HARDWARE`        | 3 | provider | post-condition not reached |
| `CLOCK_ERR_BAD_OP`          | 4 | both | unknown opcode |
| `CLOCK_ERR_NO_PROVIDER`     | 5 | devmgr only | controller_phandle has no driver |
| `CLOCK_ERR_TABLE_FULL`      | 6 | devmgr only | ClockHandleTable exhausted |
| `CLOCK_ERR_INVALID_HANDLE`  | 7 | devmgr only | handle_id not owned by caller |

The userspace `ClockClient` wrapper (see "Driver API" below)
decodes the raw `(status, value)` reply pair into a typed
`ClockError` enum via `ClockError::decode` (`clock/mod.rs:141`):
`NotSupported` carries the offending id (read from `value`),
others map 1:1.

## The binding table — `ClockHandleTable`

`lockjaw-types/src/clock_handle_table.rs:79`. The device-manager
owns one of these; it maps `(caller_token, handle_id) ->
(controller_phandle, clock_id)`. Key constants:

- `CLOCK_HANDLE_TABLE_CAP = 32` (`:38`) — total bindings across all
  drivers and clocks. Exhaustion returns `CLOCK_ERR_TABLE_FULL` on
  the next `CMD_GET_CLOCK_HANDLE` request.
- `caller_token` is the IPC identity token (see
  [`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md)).
  Two different drivers can hold handles to the *same*
  `(controller_phandle, clock_id)` and the table tracks them as
  distinct bindings keyed by token + handle_id. Cross-driver
  spoofing is impossible — devmgr verifies the caller's
  `sys_query_caller_token` matches the binding's recorded token on
  every op.

The table is host-tested for stress (`CLOCK_HANDLE_TABLE_CAP * 4`
churn cycles in `clock_handle_table.rs:243`) and exhaustion edge
cases. It lives in lockjaw-types because the cap + lookup are pure
state machine, not kernel side effects.

## Driver API — `ClockClient`

Drivers use the typed wrapper at `user/lockjaw-userlib/src/clock.rs`,
not raw `sys_call` against `CMD_GET_CLOCK_HANDLE`:

```rust
pub struct ClockClient { /* devmgr, reply, handle_id */ }

impl ClockClient {
    pub fn acquire(
        devmgr: EndpointHandle,
        reply: ReplyHandle,
        controller_phandle: u32,
        clock_id: u32,
    ) -> Result<Self, ClockError>;                       // :43

    pub fn set_rate(&self, target_hz: u64) -> Result<u64, ClockError>;  // :64
    pub fn get_rate(&self) -> Result<u64, ClockError>;                   // :69
    pub fn enable(&self) -> Result<(), ClockError>;                      // :74
    pub fn disable(&self) -> Result<(), ClockError>;                     // :79
}
```

`acquire` runs the `CMD_GET_CLOCK_HANDLE` round-trip; the four
ops run the `CLOCK_OP_*` round-trips. The wrapper holds the
`handle_id` so the caller doesn't have to track it.

The substrate's `claim_typed` does *not* construct the
`ClockClient` for the driver — what it produces is a
`ClaimedDevice<T>` whose `clock_ref: Option<ClockRef>` field
carries the already-decoded `(controller_phandle, clock_id)` from
the claim reply (`user/lockjaw-userlib/src/devmgr.rs:38`). The
driver still calls `ClockClient::acquire(...)` itself with those
two fields. The worked example is emmc2's bootstrap at
`user/emmc2-driver/src/main.rs:481-494`:

```rust
let clock_ref = ctx.clock_ref.unwrap_or_else(|| {
    puts("emmc2: DTB node has no clocks property\n");
    sys_exit();
});
let clk = ClockClient::acquire(
    ctx.devmgr_ep, ctx.reply_obj,
    clock_ref.controller_phandle, clock_ref.clock_id,
).expect("emmc2 clock acquire");
// `clk` is held in scope; drop closes the Endpoint per RAII.
```

Drivers that don't need a clock (ramfb, virtio-blk, pl011, cprman
itself) just ignore `ctx.clock_ref`.

## Server API — `ClockEngine`

Clock-provider drivers (cprman-driver today) implement the
`ClockEngine` trait and run the substrate's
`run_clock_server(&mut engine, server_ep)` event loop
(`user/lockjaw-userlib/src/clock.rs:142`). The engine handles the
four `CLOCK_OP_*` ops on its known `ClockId` variants and returns
`CLOCK_ERR_NOT_SUPPORTED` for clock_ids it doesn't model. cprman's
`ClockId` enum lives at `lockjaw-types/src/clock/cprman.rs`; new
provider drivers add their own submodule alongside.

The provider's own driver bootstrap is a Tier-A escape valve
(`boot_stub!` + manual composition) rather than `driver_main!`,
because clock-provider startup needs cross-driver coordination
that the standard helper doesn't expose. See
`user/cprman-driver/src/main.rs:229+` for the worked example, and
[`driver-howto.md`](driver-howto.md) for the escape-valve recipe.

## Source map

| File | Role |
|---|---|
| `lockjaw-types/src/clock/mod.rs` | CMD_GET_CLOCK_HANDLE, CLOCK_OP_*, CLOCK_OK/CLOCK_ERR_*, ClockError. |
| `lockjaw-types/src/clock/cprman.rs` | cprman `ClockId` enum + divider math. |
| `lockjaw-types/src/clock_handle_table.rs` | Pure binding table. CLOCK_HANDLE_TABLE_CAP=32. |
| `user/device-manager/src/main.rs` | CMD_GET_CLOCK_HANDLE handler + cross-leg routing for CLOCK_OP_*. |
| `user/cprman-driver/src/main.rs` | The (currently only) provider; Tier-A escape-valve boot. |
| `user/lockjaw-userlib/src/clock.rs` | `ClockClient`, `ClockEngine`, `run_clock_server`. |
