# IPC: Inter-Process Communication

## What IPC means in a microkernel

In a monolithic kernel (Linux, Windows), kernel subsystems communicate by calling each other's functions directly — they share an address space. In a microkernel, those subsystems are separate userspace processes with isolated address spaces. They cannot call each other. IPC is how they talk.

Lockjaw uses **synchronous rendezvous IPC**: a sender and a receiver meet at an Endpoint object. The first to arrive blocks, the second completes the transfer. No buffering, no asynchronous queues, no shared memory needed. The message is a fixed set of CPU registers transferred between threads by the kernel.

## The two ABIs

There are two distinct calling conventions in Lockjaw that look similar but work completely differently:

### 1. Rust function calls (managed by LLVM)

When Rust code calls a Rust function, the compiler handles everything. Arguments go in x0-x7 (first 8 args), return values in x0-x1, caller-saved and callee-saved registers are managed automatically. You write `let result = foo(a, b, c)` and LLVM generates the right instructions. You never think about registers.

### 2. Syscalls (managed by us)

When userspace makes a syscall via `SVC #0`, it is **not** a function call. It is a hardware trap. The CPU saves the PC and status register, switches to EL1 (kernel mode), and jumps to the exception vector. LLVM has no idea this is happening — from the compiler's perspective, the inline `asm!` block is an opaque side effect.

Userspace must manually load registers before the trap:

```rust
// Userspace code making a syscall
let result: u64;
unsafe {
    asm!(
        "svc #0",                    // trap to kernel
        in("x0") endpoint_handle,    // argument 0
        in("x1") msg_word_0,         // message register 0
        in("x2") msg_word_1,         // message register 1
        in("x3") msg_word_2,         // message register 2
        in("x8") SYS_SEND,           // syscall number
        lateout("x0") result,        // return value (overwritten by kernel)
    );
}
```

The kernel reads these registers from the saved `ExceptionContext` (the SAVE_REGS assembly macro pushes all 31 GPRs onto the kernel stack when the trap fires). The syscall handler indexes into this array:

```rust
// Kernel-side syscall handler
pub fn handle_syscall(ctx: &mut ExceptionContext) {
    let syscall_num = ctx.gpr[8];   // x8
    let handle = ctx.gpr[0];         // x0
    let msg0 = ctx.gpr[1];           // x1
    let msg1 = ctx.gpr[2];           // x2
    let msg2 = ctx.gpr[3];           // x3
    // ...
    ctx.gpr[0] = return_value;       // write return value to saved x0
}
```

When the handler returns, RESTORE_REGS pops the (potentially modified) registers back, and `eret` returns to EL0. Userspace reads the return value from `lateout("x0")`.

## How IPC message transfer works

IPC is just copying register values between two threads' saved contexts. When a sender's message needs to reach a receiver:

```rust
// The entire "message transfer" in the kernel:
receiver_ctx.gpr[0] = sender_msg[0];
receiver_ctx.gpr[1] = sender_msg[1];
receiver_ctx.gpr[2] = sender_msg[2];
receiver_ctx.gpr[3] = sender_msg[3];
```

That is it. Four u64 copies. No memory allocation, no serialization, no buffer management. When the receiver returns from its `sys_recv` syscall, those values appear in its x0-x3 registers as if by magic.

This is why microkernel IPC can be fast: the message fits in registers, and the "transfer" is a handful of memory writes to the receiver's saved state on the kernel stack.

## The userspace IPC library (future)

In Phase 7, our user test functions are naked assembly that manually loads registers. In Phase 8+, we will have a proper userspace library that wraps the raw syscall:

```rust
// Future userspace library — not kernel code
pub struct Handle(u64);

pub fn send(endpoint: Handle, msg: &[u64; 4]) -> Result<(), Error> {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x0") endpoint.0,
            in("x1") msg[0],
            in("x2") msg[1],
            in("x3") msg[2],
            in("x8") SYS_SEND,
            lateout("x0") result,
        );
    }
    if result == 0 { Ok(()) } else { Err(Error::from_code(result)) }
}

pub fn recv(endpoint: Handle) -> Result<[u64; 4], Error> {
    let r0: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x0") endpoint.0,
            in("x8") SYS_RECV,
            lateout("x0") r0,
            lateout("x1") r1,
            lateout("x2") r2,
            lateout("x3") r3,
        );
    }
    Ok([r0, r1, r2, r3])
}
```

With this library, IPC from userspace reads like normal Rust:

```rust
// Userspace server process
loop {
    let msg = recv(my_endpoint)?;
    let result = handle_request(msg);
    reply(&result)?;
}
```

The `asm!` blocks are hidden behind the library. Application code never touches registers directly.

## Endpoint lifecycle

An Endpoint is a kernel object created via the standard PageSet + create-info pattern:

1. Userspace allocates a PageSet (1 page)
2. Donates it to create an Endpoint
3. Gets a handle to the Endpoint in its handle table
4. Passes the handle number to `sys_send` or `sys_recv`

The Endpoint is a rendezvous point — it has no buffer, just state tracking who is waiting. When both a sender and receiver are present, the transfer happens immediately and both threads continue. When only one is present, it blocks until the other arrives.

## Synchronous vs asynchronous

Lockjaw's IPC is synchronous: `sys_send` blocks until a receiver picks up the message. This is the seL4/L4 model, chosen for simplicity and performance:

- **No allocation needed.** No kernel buffer to store queued messages. The message lives in registers or on the sender's saved stack until the receiver is ready.
- **Natural flow control.** A fast sender cannot overwhelm a slow receiver — it blocks automatically.
- **Predictable latency.** The worst case is known: save registers, copy 4 words, restore registers.

The tradeoff: if you want asynchronous notification (like "an IRQ happened"), you need a separate mechanism. Lockjaw will add Notification objects in Phase 9 for this — lightweight semaphore-like objects that don't transfer data, just signal.
