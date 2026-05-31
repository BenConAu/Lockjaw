# IPC: Inter-Process Communication

## What IPC means in a microkernel

In a monolithic kernel (Linux, Windows), kernel subsystems communicate
by calling each other's functions directly — they share an address
space. In a microkernel, those subsystems are separate userspace
processes with isolated address spaces. They cannot call each other.
IPC is how they talk.

Lockjaw uses **synchronous rendezvous IPC**: a sender and a receiver
meet at an Endpoint object. The first to arrive blocks, the second
completes the transfer. No buffering, no asynchronous queues, no
shared memory needed. The message is a fixed set of CPU registers
transferred between threads by the kernel.

## The register convention

IPC follows the shared syscall ABI (see [`syscalls.md`](syscalls.md)
for the broader rules):

```
Inputs:                            Outputs (two-register convention):
  x8 = syscall number              x0 = SyscallError (0 = OK)
  x0..x5 = arg slots               x1-x4 = message / return payload words
                                   x2-x4 = extra return words (some syscalls)
```

The split between `x0 = error` and `x1-x4 = payload` is the
load-bearing detail: a value-returning syscall never has to encode
"is this an error or a value?" in a single register, the way Linux's
errno-on-negative-x0 convention does.

A **message is exactly four `u64` words.** Not three, not eight —
four. But **which input registers carry the message bytes is
per-syscall** — the four message slots can fall anywhere in `x0..x5`
depending on whether the syscall also takes a handle and how many.
The three layouts in use today:

| Layout | Used by | Message regs |
|---|---|---|
| handle + msg | `sys_send` (#2), `sys_receive` (#3), `sys_recv_nb` (#14) | x1-x4 (handle in x0) |
| ep + reply + msg | `sys_call` (#4) | **x2-x5** (handles in x0 and x1) |
| msg only | `sys_reply` (#5) | **x0-x3** (no handle — server's TCB carries the binding) |

A reader writing inline asm against the SVC layer needs to consult
the per-syscall row in the next section, not the generic convention
block. The userlib wrappers (`sys_call(ep, reply, m0, m1, m2, m3)`,
`sys_reply(m0, m1, m2, m3)`, etc.) hide this by taking each message
word as a positional argument and letting the wrapper's `asm!` block
place each one in the right slot.

For `sys_send` -> `sys_receive` specifically, the kernel transfers
the message by copying `sender.gpr[1..=4]` into `receiver.gpr[1..=4]`
on the receiver's saved exception frame, and sets `receiver.gpr[0] = 0`
(SyscallError::OK). The receiver's `sys_receive` then `ERET`s back
to EL0 with the message words live in x1-x4. The transfer for
`sys_call` -> `sys_receive` is the same (`call`'s message slot is
`gpr[2..=5]` on the sender side but lands at `gpr[1..=4]` on the
receiver — the receive side is always x1-x4).

## The five IPC syscalls

| # | Name | Shape | Notes |
|---|------|-------|---|
| 2 | `sys_send` | `(ep, msg[4])` -> error | Non-blocking message put. Returns `ENDPOINT_BUSY` if no receiver was waiting. Implementation: `src/syscall/handler.rs:295`. |
| 3 | `sys_receive` | `(ep)` -> error + `msg[4]` | Blocks until a sender arrives. Implementation: `:307`. |
| 4 | `sys_call` | `(ep, reply_handle, msg[4])` -> error + `reply[4]` | Send + block for reply. The Reply object identifies the caller so the server can reply without naming an endpoint. Implementation: `:327`. |
| 5 | `sys_reply` | `(msg[4])` -> error | Reply to whichever caller is currently bound on this thread's TCB (set by the preceding `sys_receive` or `sys_call`-bound). Implementation: `:349`. |
| 14 | `sys_recv_nb` | `(ep)` -> error + `msg[4]` | Non-blocking receive. Returns `WOULD_BLOCK` if no sender is queued. Implementation: `:723`. |

A separate primitive — `sys_wait_any` (#15) — extends receive to
wait on **multiple** objects (endpoints, notifications) and/or an
absolute monotonic deadline. The wait-on-objects vocabulary is
covered in [`syscalls.md`](syscalls.md) under "wait_any (extended)".

## How a transfer actually moves bytes

The entire kernel-side transfer for `sys_send` -> `sys_receive` is:

```rust
// `sender_msg: [u64; 4]` was read out of sender.gpr[1..=4]
// by sys_send. The receiver was blocked in sys_receive, so its
// ExceptionContext is on the kernel stack.

receiver.gpr[1] = sender_msg[0];
receiver.gpr[2] = sender_msg[1];
receiver.gpr[3] = sender_msg[2];
receiver.gpr[4] = sender_msg[3];
receiver.gpr[0] = SyscallError::OK;  // 0
// scheduler::unblock_thread(receiver_tcb)
```

Four `u64` copies plus a tag write. No memory allocation, no
serialization, no buffer management. When the receiver returns from
`sys_receive`, those values appear in its x1-x4 registers as if by
magic.

That magic is why microkernel IPC can be fast: the message fits in
registers, and the "transfer" is a handful of memory writes to the
receiver's saved state on the kernel stack.

## Call / reply: identity without naming endpoints

The plain `send`/`receive` pair has no return path. A request/response
RPC built only on those primitives would need two endpoints (one for
requests, one for replies) and a way for the server to know which
caller to reply to — which means embedding a caller ID in the request
message, which the kernel cannot verify.

`sys_call` + `sys_reply` solve this with a dedicated **Reply object**:

1. The caller pre-allocates a Reply object (`sys_create_reply` over a
   PageSet) and passes its handle to `sys_call(ep, reply_handle, msg)`.
2. The kernel sends `msg` on `ep`, blocks the caller, and stamps the
   server's TCB with a binding to this Reply object.
3. The server's `sys_receive` returns with the message. The server
   can read `sys_query_caller_token` (#26) to get a per-call identity
   token — the kernel mints one fresh per `sys_call` from the Reply
   object's identity, so the server can distinguish callers without
   trusting their self-declared IDs.
4. The server processes the request, calls `sys_reply(reply_msg)`,
   and the kernel transfers `reply_msg` back to the caller's saved
   x1-x4, marks the Reply object as `Fresh` (ready for reuse), and
   unblocks the caller.

The Reply object is the kernel-managed identity. The server never
sees a "reply handle" — it just calls `sys_reply` and the binding on
its TCB routes the message back to the right caller. The Reply
object is reusable: after one round trip it transitions Fresh -> Bound
on the next `sys_call` and back to Fresh on `sys_reply`.

The identity token model is the subject of its own chapter — see
[`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md).

## Endpoint lifecycle

An Endpoint is a kernel object created via the standard PageSet +
create-info pattern:

1. Userspace allocates a one-page PageSet (`sys_alloc_pages(1, 0)`).
2. Passes its handle to `sys_create_endpoint`, which mints an
   Endpoint over the PageSet's storage and returns a fresh handle.
3. The handle is what `sys_send` / `sys_receive` / `sys_call`
   indirect through.

Internally the Endpoint object lives in the KVM pool (`src/mm/kvm.rs`)
along with all other kernel-managed objects. Userspace cannot read
its bytes; the handle is the only way to reach it.

The Endpoint is a rendezvous point — it has no buffer, just state
tracking who is waiting. When both a sender and receiver are present,
the transfer happens immediately and both threads continue. When only
one is present, it blocks until the other arrives.

## Synchronous vs asynchronous

Lockjaw's IPC is synchronous: `sys_send` blocks until a receiver
picks up the message (the call returns `ENDPOINT_BUSY` instead of
queuing). This is the seL4/L4 model, chosen for simplicity and
performance:

- **No allocation needed.** No kernel buffer to store queued
  messages. The message lives in registers or on the sender's saved
  stack until the receiver is ready.
- **Natural flow control.** A fast sender cannot overwhelm a slow
  receiver — it blocks (or fails fast, with `sys_send`'s non-blocking
  shape).
- **Predictable latency.** The worst case is known: save registers,
  copy 4 words, restore registers.

For pure async notification — "an IRQ happened", "a timer fired",
"a buffer is ready" — Lockjaw uses separate **Notification** objects
(`sys_create_notification` / `sys_signal_notification` /
`sys_wait_notification`, plus the bound-IRQ variant via
`sys_bind_irq`). Notifications are lightweight timeline semaphores:
no data transfer, just a monotonic counter and a `wait_any`-eligible
ready bit. The implementation is in `src/ipc/notification.rs`; the
syscall table entries are #9-#12 in [`syscalls.md`](syscalls.md).

## The userspace IPC library

Driver and server code never types raw `asm!` blocks. `lockjaw-userlib`
provides typed wrappers in `user/lockjaw-userlib/src/syscall.rs`. The
exposed surface is **call/reply-centric** — there is intentionally no
`sys_send` wrapper, because client/server code in Lockjaw uses
`sys_call` (which carries a Reply object for identity) rather than a
bare unidirectional send. Raw `sys_send` is available at the SVC layer
but isn't part of the userlib API.

```rust
// Call/reply (most common shape)
pub fn sys_call(
    ep: EndpointHandle, reply: ReplyHandle,
    msg0: u64, msg1: u64, msg2: u64, msg3: u64,
) -> Result<u64, SyscallError>;                          // returns x1 only

pub fn sys_call_ret4(...) -> Result<[u64; 4], SyscallError>;  // returns x1-x4

pub fn sys_reply(msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> SyscallError;

// Receive (server side)
pub fn sys_receive(ep: EndpointHandle) -> Result<u64, SyscallError>;     // x1 only
pub fn sys_receive_ret4(ep: EndpointHandle) -> Result<[u64; 4], SyscallError>;

pub fn sys_recv_nb(ep: EndpointHandle) -> Result<u64, SyscallError>;     // x1 only
```

The single-word vs `_ret4` split is deliberate: most servers carry a
single discriminant word (the request opcode) plus three payload
words, and the single-word return form keeps the common case cheap.
The `_ret4` form is for protocols that genuinely fill the message —
typically the reply path of a get-data request.

Handle types are newtypes over `u64`:
- `EndpointHandle(pub u64)` — `user/lockjaw-userlib/src/handle.rs:14`
- `ReplyHandle(pub u64)` — `user/lockjaw-userlib/src/handle.rs:22`
- `NotificationHandle`, `PageSetHandle`, etc. follow the same shape.

For drivers specifically, the substrate further hides IPC entirely
behind event-engine traits (`run_event_server` at
`user/lockjaw-userlib/src/driver_runtime.rs:733`, `run_block_server`
at `user/lockjaw-userlib/src/block.rs:176`) that own the
receive/dispatch/reply loop. See
[`../architecture/04-driver-substrate.md`](../architecture/04-driver-substrate.md)
for the regime.

## Where the kernel-side IPC code lives

| Layer | Path | Role |
|---|---|---|
| Decision protocol (pure) | `lockjaw-types/src/ipc_state.rs` | `decide_send` / `decide_receive` / `decide_call` / `decide_reply` — push -> pull conversion; the kernel handlers are match-on-decision shells. |
| Endpoint object | `src/ipc/endpoint.rs` | `EndpointObject` + state machine (`EpState::{Idle, HasWaiters, HasReceiver}` in `lockjaw-types/src/ipc_state.rs:38`). |
| Endpoint queue | `src/ipc/ep_queue.rs` | The waiter list when multiple senders or receivers are queued. |
| Notifications | `src/ipc/notification.rs` | Notification objects + IRQ binding. |
| Reply | `src/ipc/reply.rs` | ReplyObject lifecycle (Fresh -> Bound -> Fresh) + `ipc_reply` helper. |
| Caller token | `lockjaw-types/src/ipc_token.rs` | `mint_caller_token` — monotonic identity counter for `sys_query_caller_token`. |
| Syscall handlers | `src/syscall/handler.rs` | Thin shells dispatching to the above. |

The kernel-side handlers are thin shells: each one reads the args
out of the saved context, hands them to the pure decision function
in `lockjaw-types/src/ipc_state.rs`, then mechanically executes the
returned decision (transfer registers, unblock thread, etc.). The
sequencing logic itself is host-tested in lockjaw-types.
