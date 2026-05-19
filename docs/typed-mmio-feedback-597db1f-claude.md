I read the commit + substrate from cold. Honest take: the **vertical** (substrate → generated regs → userlib glue → unsafe-free driver) is genuinely well-shaped — but the driver crate itself is not yet at "holy shit." The framework gives you safe primitives; it doesn't yet give you a *short* driver. Below is what I'd preserve, what I'd fix before driver #2, and what I'd defer.

## What's right and should be protected

1. **The four-cell taxonomy (`Ro/Rw/Wo/W1c`) with no unified trait.** Codex was right; the rejection of a common `MmioCell` is the single most important type-level decision in the substrate. Every wrong-direction kernel I've ever read used a "register" trait that exposed both `read` and `write` and let the compiler shrug at `pl011.dr.write_one_byte_into_a_read_only_status_reg`. Lockjaw doesn't have that footgun. Keep it religiously.

2. **`MappedRegs::regs() -> &T` tied to `&self`, not `'static`.** Forging `'static` from a VA was the obvious shortcut and you correctly turned it down. This is what makes "the driver writes zero unsafe blocks" *actually mean* something.

3. **`CellRef<'_, T>` lifetime-bound to `&DmaPage`.** Unmap-then-use is a compile error. This is the kind of thing a future LLM driver author will try to do and the type system will catch in the editor before they even run the build. Exactly the "make illegal states unrepresentable" pattern from the principle file.

4. **TOML → `lockjaw-regs` with `gen-regs --check` drift CI + layout-offsets test in the generated file.** That's the right loop: spec is the source of truth, the test panics if codegen drifts, and CI panics if anyone forgets to regenerate. An LLM writing a new driver will edit `user/regspecs/foo.toml` and have a typed `Foo` struct fall out — that's the right ergonomic story.

5. **`claim_typed` pre-IPC layout sanity.** Rejecting `T` whose alignment/size don't fit *before* taking the claim is the right shape. Drop guard for the pageset, with comments explaining the ordering — that's exactly the discipline the principle file asks for and it's well executed.

## What will bite the next driver author

These are not correctness bugs; they're **friction**, and the "holy shit" experience lives in friction.

1. **`_start()` is 210 lines of `let x = match … { Err(_) => { puts("foo FAILED\n"); halt() } };`.** Boot → reply object → `sys_call_ret4` bootstrap → probe-by-hash-loop → `claim_typed` → magic check → init state machine → IRQ bind → server. Driver #2 will copy/paste this verbatim. The framework promises "no unsafe in driver"; it does not yet promise "short driver." A `driver_main!(BlockDriver, |ctx: DriverCtx<VirtioMmio>| { ... })` macro that hides the boot stubs, the probe loop, the bootstrap-call/reply allocation, the `LOCKJAW_HASH_SECTION` boilerplate, and the `#[allow(unsafe_code)]` attribute leaks — that should land *in this sweep*, before driver #2. The Phase 8 note already acknowledges this; deferring it means each new driver re-imprints the wrong shape.

2. **VirtIO init is the textbook plan-apply state machine — and it's a free-form sequence of `set_status` calls.** Reset → ACK → DRIVER → negotiate → FEATURES_OK → setup_queue → DRIVER_OK is spec 3.1.1. Lockjaw's *own* `docs/patterns/plan-apply.md` was written for exactly this shape. As coded today a driver can skip a step, write `Status::DRIVER_OK` directly, or call `negotiate` before `ACK`, and the compiler shrugs. A typed builder where each method returns the next state type would make the spec unrepresentable-illegal:
   ```rust
   let dev = VirtioMmioInit::reset(mmio)
       .acknowledge()
       .driver()
       .negotiate(BLK_DRIVER_WANTED)?       // requires VERSION_1
       .features_ok()?
       .setup_queue(0, vq)
       .driver_ok();                          // -> VirtioMmio<Running>
   ```
   This is the single highest-leverage "holy shit" change. It also lets `do_io` take `&VirtioMmio<Running>` so issuing I/O against an un-init'd device is a type error. Doing it once in lockjaw-userlib pays for net, gpu, console, …

3. **`alloc_chain3(buf0, len0, flags0, buf1, len1, flags1, buf2, len2, flags2)` — 9 positional args.** This is the exact shape of the bug it's trying to prevent. Swap `buf1`/`buf2` and it'll compile, run, and corrupt sectors. A struct-of-segments:
   ```rust
   vq.submit(&[
       Segment::device_readable(req_page.pa_of::<VirtioBlkReqHeader>()),
       Segment::for_op(req_type, data_pa, data_len),
       Segment::device_writable(req_page.pa_of_status()),
   ])
   ```
   reads better, scales to non-3-segment chains (config writes need 2, ramfb scanout will need different), and the descriptor-flags computation moves into the framework once.

4. **`const STATUS_BYTE_OFFSET: u64 = 16;` next to a comment "VirtioBlkReqHeader is 16 bytes."** That is the magic-number-where-a-type-exists pattern that `CLAUDE.md` opens with. `size_of::<VirtioBlkReqHeader>() as u64` is the literal example. Cheap fix, but it's a tell that the surrounding "request header + status byte in one DMA page" pattern wants a `ReqStatusPage` type with `header_cell()` / `status_cell()` accessors so the offset doesn't live in driver code at all.

5. **`DmaPage` aliasing is a docs invariant, not type-enforced.** The module-level comment says so. For drivers that statically partition a page (virtio-blk does: header@0, status@16), a `TypedDmaPage<L>` where `L` is a `#[repr(C)]` struct of `DmaCell<T>` fields would close this exactly the way `MappedRegs<T>` closed it for MMIO. Same generator could emit it from a TOML layout. The dynamic `cell(offset)`/`slice(offset, n)` API stays for genuinely-runtime cases (virtqueue rings sized at negotiation time).

6. **`do_io`'s IRQ → ack → poll ordering is a documented pattern hiding in driver code.** ACK after drain, drain before ACK — virtio races live here. This belongs as `Virtqueue::wait_for_completion(&mut self, &irq_notif, &mut threshold, mmio)`. Right now every future virtio driver will reimplement the loop and at least one will reorder the ack and the poll.

7. **`DmaPage::unmap(self) -> Result` with no `Drop`.** Forget to call it → leak until process exit. Principle file is explicit: "drop guards for resource cleanup on failure paths, not manual rollback." This is the one place in the substrate that violates its own rule. `impl Drop for DmaPage` calling `sys_unmap_pages + VMEM.free` (ignoring errors, since drop can't return), with explicit `.unmap() -> Result<(), SyscallError>` retained for "I want the error back." Consistent with the project's discipline elsewhere.

8. **`#[allow(unsafe_code)]` × 3 on the boot stubs.** Real but small. Roll into the `driver_main!` macro (#1) and the driver crate becomes truly `#![forbid(unsafe_code)]`, not `#![deny]` with carve-outs. That's the asterisk-free version of the claim.

9. **Hand-rolled bitflag boilerplate in `virtio_mmio.rs`** (`BitOr`/`BitAnd`/`Not`/`empty()`/`bits()`/`contains()` — ~25 lines per flag type). Move into codegen so a `[[flags]] name="Status"` table in the TOML produces all of it. PL011's interrupt-mask register is going to want the same shape; uart-driver will too.

10. **The 30-line stack-buffer hex dump in selftest.** Symptom, not cause: `lockjaw-userlib` doesn't yet have `puts_hex_bytes_atomic(&[u8])`. Small fix; conspicuous on a "look how short a driver is" demo.

## Recommended split

**Do now, before driver #2** (these compound — every driver after this commit inherits the shape you set here):
- `driver_main!` macro + bootstrap/probe helper (item 1, 8)
- Typed VirtIO init state machine (item 2)
- `Segment`-based virtqueue submission + replace magic 16 with `size_of` (items 3, 4)
- `Drop for DmaPage` (item 7)
- `Virtqueue::wait_for_completion` (item 6)

**Defer to a punch-list pass after a couple more drivers** (you want real second/third use sites before generalizing):
- `TypedDmaPage<L>` codegen (item 5) — wait for emmc2 / ramfb to see what shapes recur
- Bitflag codegen in `gen-regs` (item 9) — wait until 3rd flag type appears
- Atomic hex/decimal output helpers in userlib (item 10)

The thing I'd resist: do *not* generalize `BlockEngine`/`run_block_server` into a "generic device server" yet. It's the cleanest part of this commit precisely because it's narrow. Let the gpu/net drivers prove their server shapes first; a premature `DeviceEngine` trait would force LCD-vs-DMA-vs-block into one signature and undo what works.
