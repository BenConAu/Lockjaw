# Handle Identity Tokens

> *The token identifies the endpoint that you give a client.*
>
> — Ben, derivation conversation, 2026-05-12

Lockjaw's IPC servers need to tell their clients apart. The kernel
issues an **identity token** on every endpoint handle export. This
chapter pins down the requirement that motivates the model, the
implementation that meets it, and the design choices that don't
follow from the requirement so that future work can re-examine them
honestly.

The history matters: the first implementation copied lineage on
re-export ("a delegated handle preserves the original recipient's
identity"). That was an unstated and unjustified design choice that
fell out of treating "first export from master" as the special case
for minting. It blocked the M0c clock-cap work, and a re-derivation
from first principles produced a cleaner, more compositional model.
This chapter is the result of that re-derivation, written so future
us can ramp in fast if a similar question surfaces.

## The requirement

**A server must be able to distinguish two clients as having
distinct identities.**

That is the entire requirement. Everything below is consequence and
implementation detail; nothing else was ever specified. Anything you
read in this chapter that doesn't reduce to that statement is a
choice, not a constraint.

What follows from the requirement, by deduction:

1. **The token must be kernel-issued.** A client-supplied token would
   let one client impersonate another by lying. Forgery defeats the
   "distinguish" property.
2. **The token must be stable for the same client across multiple
   calls.** Otherwise the server can't accumulate per-client state
   (the whole reason you wanted distinct identities).
3. **Different clients must get different tokens.** This is just
   restating the requirement.

That is the closure of "what the requirement forces." Everything
else — the discriminant value, the relationship to handle creation,
re-export semantics, the type used to represent the token — is a
design choice. We make those choices below, and we *name them as
choices*.

## What does NOT follow from the requirement

The original implementation got two design choices wrong by
conflating them with the requirement.

### Re-export semantics

When process A holds a sender handle to endpoint E and uses
`sys_export_handle` to give a copy to process B, what identity does
B's handle carry?

- Option (i): **Same identity as A's.** B looks like A to the
  server.
- Option (ii): **Fresh identity.** B looks distinct from A.

The requirement does not pick. Both options preserve "the server
can distinguish two clients" — they just disagree about whether A
and B count as one client or two.

The original implementation chose (i) ("lineage preservation").
That would matter if Lockjaw needed transitive delegation — passing
a handle to a child process so the child can speak to a server "on
behalf of" the parent. We don't need that today, and reaching for
it imports semantics from L4-style cap systems without the use
cases that justify it there.

Lockjaw chooses **(ii)**: every export is a distinct gift event
with its own minted token. Properties that fall out:

- Handle-as-identity. Two distinct handles to the same endpoint
  always represent two distinct clients, regardless of how they
  were obtained or who held them in between.
- Compositional brokers. A process that re-exports a handle to N
  recipients (e.g., `device-manager` handing out clock-control
  handles) gives each recipient a distinct identity that the server
  can route on. No "master holder" needs to be in the loop.
- No accidental escalation. A process can't pretend to be its
  parent by re-exporting a handle the parent gave it; the
  re-export creates a new identity scoped to the receiver.

### The "master" sentinel

The endpoint creator has to hold *some* handle to receive on. The
requirement does not say what the kernel should call this handle or
how it should be represented. The original implementation used
`caller_token = 0` as a magic value meaning "creator's
receive-only handle." That choice is fine, but it should be
encoded in the type system rather than as a magic number.

Lockjaw chooses to encode it as `Option<NonZeroU64>`:

- `None` — master / receive-only. Held by the creator. Send and
  call paths reject `None` at pattern-match time.
- `Some(token)` — sender handle. Has a kernel-issued identity. Both
  send/call and receive work on a sender handle (a process can be
  a server and a client at once).

`NonZeroU64` makes the "sender handle with token 0" state
unrepresentable. The send/call rejection becomes
`HandleKind::Endpoint { caller_token: None, .. } => INVALID_PARAMETER`,
not `if caller_token == 0`. This kills the sentinel-magic-value
class of bug at the type level.

## The implementation

Three pieces, each with one job.

### 1. The pure mint logic

`lockjaw_types::ipc_token::mint_caller_token` owns the arithmetic:

```rust
pub fn mint_caller_token(next_token: u64) -> (NonZeroU64, u64) {
    let token = NonZeroU64::new(next_token)
        .expect("next_token must be initialized to 1");
    let next = next_token.checked_add(1)
        .expect("next_token wrap");
    (token, next)
}
```

It is host-testable; the invariants (counter starts at 1, monotonic,
panics on wrap-to-0) are exercised by unit tests next to the
function. No kernel state, no KVM, no `EndpointObject` —
just a pure function.

### 2. The kernel mint helper

`crate::ipc::endpoint::mint_caller_token` is a thin wrapper that
loads the counter from an `EndpointObject`, calls the pure helper,
writes the new counter back, and returns the minted token.

It is the single source of truth for *all* token minting. Both
`sys_export_handle` and `create_process` handle copy go through it.
The deleted special case was: "if the source handle is the master
(token == 0), mint; else copy." The replacement is "always mint,
period."

### 3. The handle-kind variant

`HandleKind::Endpoint { kva, caller_token: Option<NonZeroU64> }`.
Match sites that need the token use `Some(t)`; sites that don't
care use `..`. The send/call paths in `cap/object_ops.rs` are the
canonical "needs-the-token" sites; receive uses `..`.

### What `caller_token=0` *used to* mean and doesn't anymore

The `caller_token: u64` field was overloaded: 0 meant "master"; any
nonzero value meant "sender with that identity." The `if
caller_token == 0` check appeared in three places (sys_export_handle,
create_process handle copy, ipc send/call rejection) — three sites
each carrying a copy of the master/sender invariant by hand.

After the change: there is no overload. Master is `None`, sender
is `Some(NonZeroU64)`. The mint helper returns `NonZeroU64`, so
the type system makes "sender with token 0" unrepresentable. The
master/sender distinction lives in one place — the pattern of the
variant payload — instead of in three coordinated checks.

## How this lands in the bootstrap chain

Lockjaw's bootstrap pattern has init create endpoints and export
them to drivers. Under the new always-mint rule:

1. Init calls `sys_create_endpoint` → init's handle table gets
   `Endpoint { kva, caller_token: None }`. Init can receive on
   this handle. It cannot send/call.
2. Init calls `sys_export_handle(my_endpoint)` while replying to a
   driver's bootstrap call. The kernel mints a fresh
   `NonZeroU64(t1)` from the endpoint's counter and inserts
   `Endpoint { kva, caller_token: Some(t1) }` into the driver's
   handle table.
3. The driver receives, and can now send/call through this handle.
   When the driver calls, the server sees `caller_token = t1` via
   `sys_query_caller_token`.
4. If init exports the same endpoint to a *second* driver, that
   driver gets a fresh `t2 != t1`. The server can distinguish the
   two drivers.
5. If a driver re-exports its sender handle to a child process, the
   child's handle has a fresh `t3 != t1`. The server sees the
   child as a distinct client — not as "the original driver."

Property (5) is what unblocks M0c: `device-manager` can hand out
clock-control handles to many drivers, each carrying a distinct
identity, without any new kernel surface or special "minting
authority" in the loop.

## Things to revisit if the requirement changes

If Lockjaw ever needs transitive delegation ("the child should look
like the parent to the server, because the child is acting on the
parent's behalf"), the right answer is *not* to revert this chapter.
It is to add an explicit syscall — e.g., `sys_export_handle_as`
that names the identity to use — and gate it on a right held only
by trusted brokers. Transitive delegation is a meaningful feature
that deserves its own surface, not a side effect of how re-export
happens to be implemented.

If the counter could ever be exhausted (2^64 mints per endpoint),
revisit the wrap-panic: switch to a different identity scheme
(e.g., process-tagged tokens) before the panic fires. Realistically
unreachable in current Lockjaw, but the panic is intentional: it
prevents a 0 token from ever leaking into a sender handle, which
would otherwise look like the master and silently re-route IPC.

## What this chapter is for

If you are reading this because something in the IPC identity model
seems off, the procedure is:

1. Re-read **The requirement** above. Confirm that the property you
   need is or is not the property the requirement names.
2. If the property *is* the requirement, the implementation should
   already have you covered — go read the three pieces.
3. If the property is *not* the requirement, you are in design
   space. Make the choice explicitly and add it to **What does NOT
   follow from the requirement** above with the rationale, the same
   way re-export semantics and the master sentinel are documented
   here.

The pattern that brought us here was: a design decision was made
silently and pattern-matched into kernel logic without the
requirement being re-derived. The fix isn't more careful coding —
it's writing the requirement down so the next decision can be tested
against it.
