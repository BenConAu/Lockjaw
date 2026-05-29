#![no_std]
#![no_main]
// Driver-crate body writes zero `unsafe` blocks AND zero
// `#[allow(unsafe_code)]` attributes. The macro-generated boot
// stubs in `lockjaw_userlib::boot_stub!` are the single audited
// location for the boot-entry attributes; the macro expansion is
// the only place `#[allow(unsafe_code)]` appears for this build.
//
// `#![deny]` (not `#![forbid]`) so the macro-emitted per-item
// allows on `#[no_mangle]` and `#[link_section]` are honoured.
// Acceptance grep: `grep -rn 'allow(unsafe_code)' user/virtio-blk-driver/src/`
// MUST return nothing — driver source contains zero allows; all
// allows are in the lockjaw-userlib macro body.
#![deny(unsafe_code)]

use core::mem::size_of;
use lockjaw_userlib::block::{BlockEngine, BlockError, BlockInfo, run_block_server};
use lockjaw_userlib::dma::{
    alloc_dma_backing, close_dma_backing, BorrowedDmaMapping, BuddyOrigin, DmaMappingView,
    OwnedDmaMapping,
};
use lockjaw_userlib::driver_runtime::DriverCtx;
use lockjaw_userlib::handle::{NotificationHandle, PageSetHandle};
use lockjaw_userlib::virtio::{VirtioMmio, VirtioTransportInit};
use lockjaw_userlib::virtio_blk::VirtioBlkDevice;
use lockjaw_userlib::virtio_driver_main;
use lockjaw_userlib::virtqueue::{Segment, Virtqueue};
use lockjaw_userlib::{put_decimal, puts, sys_debug_puts, sys_exit, MapMemoryAttribute, PAGE_SIZE};
use lockjaw_types::virtio::{
    virtqueue_layout, BLK_DRIVER_WANTED, DEVICE_ID_BLOCK, VirtioBlkReqHeader,
    VIRTIO_BLK_S_OK, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};

// ---------------------------------------------------------------------------
// VirtIO block engine
// ---------------------------------------------------------------------------

/// Maximum number of tracked DMA buffers.
const MAX_DMA_BUFFERS: usize = 8;

/// Virtqueue size to use (must be <= device's QUEUE_NUM_MAX —
/// `setup_queue` clamps if the device offers less).
const QUEUE_SIZE: u16 = 128;

struct VirtioBlkEngine {
    /// virtio-blk device wrapper. Owns the post-init `VirtioTransport`
    /// underneath and exposes blk-specific config-space accessors
    /// (capacity, etc.) without polluting the generic transport.
    device: VirtioBlkDevice,
    /// Virtqueue for block I/O requests.
    vq: Virtqueue,
    /// Block device capacity in sectors (cached from `device.read_capacity()`
    /// at init).
    capacity: u64,
    /// Owns the request/status DMA mapping. The request header lives
    /// at offset 0; the status byte lives immediately after at
    /// `size_of::<VirtioBlkReqHeader>()`. Views are computed per-use
    /// so they stay lifetime-bound to `&req_page`.
    req_page: OwnedDmaMapping<BuddyOrigin>,
    /// IRQ notification handle (for waiting on completion).
    irq_notif: NotificationHandle,
    /// IRQ threshold (monotonic, incremented after each IRQ).
    irq_threshold: u64,
    /// Tracked DMA buffer physical addresses, indexed by buffer slot.
    dma_buffers: [DmaBuffer; MAX_DMA_BUFFERS],
    dma_count: usize,
}

#[derive(Clone, Copy)]
struct DmaBuffer {
    ps: PageSetHandle,
    pa: u64,
    sector_count: u64,
}

const DMA_BUFFER_EMPTY: DmaBuffer = DmaBuffer {
    ps: PageSetHandle(0),
    pa: 0,
    sector_count: 0,
};

impl BlockEngine for VirtioBlkEngine {
    fn info(&self) -> BlockInfo {
        BlockInfo {
            capacity_sectors: self.capacity,
            sector_size: 512,
            // virtio-blk allocates buffers via `sys_alloc_pages_contiguous`
            // (Buddy origin), so the client must map them cacheable.
            buffer_attribute: MapMemoryAttribute::Normal,
        }
    }

    fn alloc_buffer(&mut self, sector_count: u64) -> Result<PageSetHandle, BlockError> {
        if self.dma_count >= MAX_DMA_BUFFERS {
            return Err(BlockError::AllocFailed);
        }
        let pages = (sector_count * 512 + PAGE_SIZE - 1) / PAGE_SIZE;
        let backing = alloc_dma_backing(pages).map_err(|_| BlockError::AllocFailed)?;
        // Track this buffer in the slot table; on success the slot
        // takes ownership of the pageset (close happens in free_buffer).
        // On failure (table full) we close immediately so the
        // allocation doesn't leak.
        for slot in self.dma_buffers.iter_mut() {
            if slot.pa == 0 {
                *slot = DmaBuffer { ps: backing.pageset, pa: backing.pa, sector_count };
                self.dma_count += 1;
                return Ok(backing.pageset);
            }
        }
        close_dma_backing(backing.pageset);
        Err(BlockError::AllocFailed)
    }

    fn read(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        let buf = self.find_buffer(buffer)?;
        if count > buf.sector_count || sector + count > self.capacity {
            return Err(BlockError::InvalidParameter);
        }
        self.do_io(VIRTIO_BLK_T_IN, sector, count, buf.pa)
    }

    fn write(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        let buf = self.find_buffer(buffer)?;
        if count > buf.sector_count || sector + count > self.capacity {
            return Err(BlockError::InvalidParameter);
        }
        self.do_io(VIRTIO_BLK_T_OUT, sector, count, buf.pa)
    }

    fn free_buffer(&mut self, buffer: PageSetHandle) {
        for slot in self.dma_buffers.iter_mut() {
            if slot.ps.0 == buffer.0 && slot.pa != 0 {
                close_dma_backing(slot.ps);
                *slot = DMA_BUFFER_EMPTY;
                self.dma_count -= 1;
                return;
            }
        }
    }
}

impl VirtioBlkEngine {
    fn find_buffer(&self, ps: PageSetHandle) -> Result<DmaBuffer, BlockError> {
        for slot in &self.dma_buffers {
            if slot.ps.0 == ps.0 && slot.pa != 0 {
                return Ok(*slot);
            }
        }
        Err(BlockError::InvalidBuffer)
    }

    /// Perform a block I/O operation (read or write).
    /// Builds a 3-segment chain (header + data + status), submits to
    /// the virtqueue, waits for the device to signal completion via
    /// the IRQ notification, then checks the status byte.
    fn do_io(&mut self, req_type: u32, sector: u64, count: u64, data_pa: u64)
        -> Result<(), BlockError>
    {
        let data_len = (count * 512) as u32;
        // Byte offset of the status byte: immediately after the
        // request header. `size_of::<T>()` instead of a magic 16 —
        // CLAUDE.md "Types over constants" principle.
        const STATUS_OFFSET: u64 = size_of::<VirtioBlkReqHeader>() as u64;

        // Write request header to the dedicated header page. The
        // generated VirtioBlkReqHeader constructor takes (req_type,
        // sector) — the spec's `reserved` field is mandated zero
        // and the wirespec marks it `default = 0`, so the constructor
        // omits it from the signature.
        self.req_page
            .cell::<VirtioBlkReqHeader>(0)
            .write(VirtioBlkReqHeader::new(req_type, sector));
        // Clear status byte before submission so we can distinguish
        // device-set status from leftover bits. Status byte lives
        // after the header in the same page.
        self.req_page.cell::<u8>(STATUS_OFFSET).write(0xFF);

        // Build a 3-segment chain. The framework computes the
        // descriptor flags from each segment's direction; the driver
        // never sees VIRTQ_DESC_F_*.
        let segments = [
            Segment::readable(self.req_page.pa(), size_of::<VirtioBlkReqHeader>() as u32),
            if req_type == VIRTIO_BLK_T_IN {
                Segment::writable(data_pa, data_len)
            } else {
                Segment::readable(data_pa, data_len)
            },
            Segment::writable(self.req_page.pa() + STATUS_OFFSET, 1),
        ];

        // Submit and notify device.
        let head = self.vq.submit_chain(&segments).ok_or(BlockError::AllocFailed)?;
        self.device.transport().queue_notify(0);

        // Wait for completion; the IRQ + ack + poll loop is baked
        // into the substrate (`Virtqueue::wait_for_completion`) so
        // each virtio driver doesn't reimplement (and at least one
        // would inevitably reorder the ack and the poll — a known
        // virtio race shape).
        self.vq
            .wait_for_completion(self.irq_notif, &mut self.irq_threshold, self.device.transport())
            .map_err(|_| BlockError::IoError)?;

        // Free the descriptor chain.
        self.vq.free_chain(head);

        // Check status byte.
        let status = self.req_page.cell::<u8>(STATUS_OFFSET).read();
        if status != VIRTIO_BLK_S_OK {
            return Err(BlockError::IoError);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Driver main — invoked by the driver_main! macro after boot, probe,
// claim, IRQ bind. ctx exposes typed regs + irq_notif + server_ep.
// ---------------------------------------------------------------------------

const VQ_INDEX: u16 = 0;

fn virtio_blk_main(ctx: DriverCtx<VirtioMmio>) -> ! {
    // Wrap the MMIO region in the typed init builder. `reset()`
    // verifies MAGIC_VALUE + VERSION up front, so reaching
    // `.acknowledge()` proves we're talking to a modern virtio
    // device.
    let init = match VirtioTransportInit::reset(ctx.regs) {
        Ok(i) => i,
        Err(_) => { puts("blk: not virtio\n"); sys_exit(); }
    };

    // Linear init: each step returns the next state's type, so
    // calling out of order is a compile error.
    let init = init.acknowledge().driver();
    let init = match init.negotiate(BLK_DRIVER_WANTED) {
        Ok(i) => i,
        Err(_) => { puts("blk: feature negotiation FAILED\n"); sys_exit(); }
    };
    let init = match init.features_ok() {
        Ok(i) => i,
        Err(_) => { puts("blk: FEATURES_OK rejected\n"); sys_exit(); }
    };

    // setup_queue inverts the dependency: it knows queue_num_max
    // (only readable after features_ok), passes it to the factory
    // closure which allocates the backing region and builds the
    // virtqueue, then programs the device-side PAs from the
    // returned vq. Host-side allocation failure surfaces as a
    // distinct VirtioInitError::BackingAllocFailed so the log
    // line doesn't lie ("queue unavailable" would be a false
    // device-state message).
    let (init, vq) = match init.setup_queue(VQ_INDEX, |max| {
        let qs = if (QUEUE_SIZE as u32) <= max as u32 { QUEUE_SIZE } else { max };
        let total = virtqueue_layout(qs).total_size as u64;
        let pages = (total + PAGE_SIZE - 1) / PAGE_SIZE;
        let backing = OwnedDmaMapping::alloc_contiguous(pages).map_err(|_| {
            lockjaw_userlib::virtio::VirtioInitError::BackingAllocFailed { pages }
        })?;
        Ok(Virtqueue::new(backing, qs))
    }) {
        Ok(pair) => pair,
        Err(_) => { puts("blk: setup_queue FAILED\n"); sys_exit(); }
    };

    let device = VirtioBlkDevice::new(init.driver_ok());

    // Read capacity via the synthesized u64 accessor (Phase 4A.2),
    // exposed by the device wrapper (blk-specific config space).
    let capacity = device.read_capacity();
    puts("blk: initialized, capacity=");
    put_decimal(capacity);
    puts(" sectors\n");

    // Allocate a page for request headers + status bytes (reused per I/O).
    let req_page = match OwnedDmaMapping::alloc() {
        Ok(p) => p,
        Err(_) => { puts("blk: alloc req page FAILED\n"); sys_exit(); }
    };

    let mut engine = VirtioBlkEngine {
        device,
        vq,
        capacity,
        req_page,
        irq_notif: ctx.irq_notif,
        // Initial threshold contract owned by BoundIrq's docstring;
        // ctx.irq_initial_threshold is its type-level surface here.
        irq_threshold: ctx.irq_initial_threshold,
        dma_buffers: [DMA_BUFFER_EMPTY; MAX_DMA_BUFFERS],
        dma_count: 0,
    };

    // Self-test: read sector 0 and print the first 16 bytes.
    let test_buf = engine.alloc_buffer(1).expect("blk: selftest alloc");
    let test_page = match BorrowedDmaMapping::map_existing(test_buf, 1) {
        Ok(p) => p,
        Err(_) => { puts("blk: selftest map FAILED\n"); sys_exit(); }
    };
    test_page.zero();
    match engine.read(0, 1, test_buf) {
        Ok(()) => {
            // Stack-buffer hex dump kept inline — small helper to
            // emit `sys_debug_puts` atomically so concurrent driver
            // output can't interleave. 16 bytes × (2 hex + 1 space)
            // - trailing space = 47 chars, + prefix + "]\n".
            let prefix = b"blk: selftest read OK, sector 0 = [";
            let mut buf = [0u8; 35 + 47 + 2];
            let mut len = 0;
            for &c in prefix { buf[len] = c; len += 1; }
            let test_slice = test_page.slice::<u8>(0, 16);
            for i in 0..16 {
                let b = test_slice.read(i);
                let hi = (b >> 4) & 0xF;
                let lo = b & 0xF;
                if i > 0 { buf[len] = b' '; len += 1; }
                buf[len] = if hi < 10 { b'0' + hi } else { b'a' + hi - 10 };
                len += 1;
                buf[len] = if lo < 10 { b'0' + lo } else { b'a' + lo - 10 };
                len += 1;
            }
            buf[len] = b']'; len += 1;
            buf[len] = b'\n'; len += 1;
            sys_debug_puts(&buf[..len]);
        }
        Err(_) => { puts("blk: selftest read FAILED\n"); }
    }
    drop(test_page);
    engine.free_buffer(test_buf);

    puts("blk: serving\n");
    run_block_server(&mut engine, ctx.server_ep)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("blk: PANIC\n");
    sys_exit();
}

// ---------------------------------------------------------------------------
// Driver boot — generated by the macro. `virtio_driver_main!` is the
// virtio-family analogue of `driver_main!`: it uses
// `virtio::virtio_driver_init` instead of `standard_driver_init`,
// which loops probe → claim → validate-magic-and-DeviceID →
// release-if-wrong → try next. This correctly skips phantom empty
// virtio-mmio slots in QEMU (whose magic is 0, not 0x74726976) AND
// puts the DeviceID match in the virtio family layer instead of the
// generic probe path. The macro expansion is the single location
// where the driver build carries `#[allow(unsafe_code)]`; the
// driver crate body is unsafe-free.
// ---------------------------------------------------------------------------

virtio_driver_main! {
    name = "blk",
    hash = LOCKJAW_SOURCE_HASH,
    device_id = DEVICE_ID_BLOCK,
    main = virtio_blk_main,
}
