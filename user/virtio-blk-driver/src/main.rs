#![no_std]
#![no_main]
// The `unsafe_code` lint covers `unsafe { ... }` blocks AND attributes
// like `#[no_mangle]` / `#[link_section]` that affect ABI. The driver
// proper writes zero unsafe blocks; only the boot-entry stubs below
// need attribute-level allows. We use `deny` (not `forbid`) so the
// per-item `allow` works; Phase 8 will move the boot stubs into a
// macro inside lockjaw-userlib so even the per-item allows go away.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[allow(unsafe_code)]
#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::devmgr::claim_typed;
use lockjaw_userlib::dma::DmaPage;
use lockjaw_userlib::block::{BlockEngine, BlockInfo, BlockError, run_block_server};
use lockjaw_userlib::virtqueue::Virtqueue;
use lockjaw_mmio::region::MappedRegs;
use lockjaw_regs::virtio_mmio::{Status, VirtioMmio};
use lockjaw_types::virtio::{
    FeatureNegotiation, VirtioBlkReqHeader, virtqueue_layout,
    BLK_DRIVER_WANTED, DEVICE_ID_BLOCK, VIRTIO_BLK_S_OK, VIRTIO_BLK_T_IN,
    VIRTIO_BLK_T_OUT, VIRTIO_MMIO_MAGIC_VALUE, VIRTQ_DESC_F_WRITE,
};
use lockjaw_types::device::{
    VIRTIO_MMIO_HASH, CMD_PROBE_DEVICE, PROBE_OK, PROBE_END,
};

// ---------------------------------------------------------------------------
// VirtIO block engine
// ---------------------------------------------------------------------------

/// Maximum number of tracked DMA buffers.
const MAX_DMA_BUFFERS: usize = 8;

/// Virtqueue size to use (must be <= device's QUEUE_NUM_MAX).
const QUEUE_SIZE: u16 = 128;

/// Byte offset of the status byte inside the shared request/status page.
/// The VirtioBlkReqHeader is 16 bytes; the status byte immediately follows.
const STATUS_BYTE_OFFSET: u64 = 16;

struct VirtioBlkEngine {
    /// Typed MMIO transport region.
    mmio: MappedRegs<VirtioMmio>,
    /// Virtqueue for block I/O requests.
    vq: Virtqueue,
    /// Block device capacity in sectors.
    capacity: u64,
    /// Owns the request/status DMA page. The request header and the
    /// status byte are different offsets within this page; views are
    /// computed per-use via `req_page.cell::<T>(offset)` so they
    /// borrow `&req_page` and can't outlive it.
    req_page: DmaPage,
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
            // virtio-blk allocs buffers via `sys_alloc_pages_contiguous`
            // (Buddy origin), so the client must map them cacheable.
            buffer_attribute: MapMemoryAttribute::Normal,
        }
    }

    fn alloc_buffer(&mut self, sector_count: u64) -> Result<PageSetHandle, BlockError> {
        if self.dma_count >= MAX_DMA_BUFFERS {
            return Err(BlockError::AllocFailed);
        }
        let pages = (sector_count * 512 + PAGE_SIZE - 1) / PAGE_SIZE;
        // Guard closes the handle on drop if any subsequent step fails.
        let guard = handle::PageSetGuard::new(
            sys_alloc_pages_contiguous(pages).map_err(|_| BlockError::AllocFailed)?
        );
        let pa = sys_query_pageset_phys(guard.handle(), 0)
            .map_err(|_| BlockError::AllocFailed)?;

        // Track this buffer, then disarm the guard.
        for slot in self.dma_buffers.iter_mut() {
            if slot.pa == 0 {
                let ps = guard.take();
                *slot = DmaBuffer { ps, pa, sector_count };
                self.dma_count += 1;
                return Ok(ps);
            }
        }
        // Guard drops here and closes the handle.
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
                sys_close_handle(slot.ps);
                *slot = DMA_BUFFER_EMPTY;
                self.dma_count -= 1;
                return;
            }
        }
    }
}

impl VirtioBlkEngine {
    fn regs(&self) -> &VirtioMmio { self.mmio.regs() }

    fn find_buffer(&self, ps: PageSetHandle) -> Result<DmaBuffer, BlockError> {
        for slot in &self.dma_buffers {
            if slot.ps.0 == ps.0 && slot.pa != 0 {
                return Ok(*slot);
            }
        }
        Err(BlockError::InvalidBuffer)
    }

    /// Perform a block I/O operation (read or write).
    /// Builds a 3-descriptor chain, submits to virtqueue, waits for IRQ.
    fn do_io(&mut self, req_type: u32, sector: u64, count: u64, data_pa: u64)
        -> Result<(), BlockError>
    {
        let data_len = (count * 512) as u32;

        // Write request header to the dedicated header page.
        self.req_page.cell::<VirtioBlkReqHeader>(0).write(VirtioBlkReqHeader {
            req_type,
            reserved: 0,
            sector,
        });

        // Status byte lives after the header in the same page.
        let status_pa = self.req_page.pa() + STATUS_BYTE_OFFSET;
        // Clear status byte before submission so we can distinguish
        // device-set status from leftover bits.
        self.req_page.cell::<u8>(STATUS_BYTE_OFFSET).write(0xFF);

        // Data descriptor flags: device-writable for reads, device-readable for writes.
        let data_flags = if req_type == VIRTIO_BLK_T_IN {
            VIRTQ_DESC_F_WRITE
        } else {
            0
        };

        // Allocate 3-descriptor chain: header + data + status.
        let head = self.vq.alloc_chain3(
            self.req_page.pa(), 16, 0,             // header: device-readable
            data_pa, data_len, data_flags,          // data
            status_pa, 1, VIRTQ_DESC_F_WRITE,       // status: device-writable
        ).ok_or(BlockError::AllocFailed)?;

        // Submit and notify device.
        self.vq.submit(head);
        self.regs().write_queue_notify(0);

        // Wait for completion via IRQ notification.
        loop {
            // Poll first in case it completed before we wait.
            if let Some((_id, _len)) = self.vq.poll_used() {
                break;
            }
            // Wait for IRQ.
            match sys_wait_notification(self.irq_notif, self.irq_threshold) {
                Ok(_) => {}
                Err(_) => return Err(BlockError::IoError),
            }
            self.irq_threshold += 1;
            // ACK the interrupt: read the cause bits, then write them
            // back to clear (W1C semantics on interrupt_ack).
            let pending = self.regs().read_interrupt_status();
            self.regs().clear_interrupt_ack(pending);
        }

        // Free the descriptor chain.
        self.vq.free_chain(head);

        // Check status byte.
        let status = self.req_page.cell::<u8>(STATUS_BYTE_OFFSET).read();
        if status != VIRTIO_BLK_S_OK {
            return Err(BlockError::IoError);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("blk: starting\n");

    // Allocate Reply object for outbound IPC.
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("blk: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: call init on handle 0 to receive server endpoint + devmgr endpoint.
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("blk: bootstrap FAILED\n"); halt(); }
    };
    let server_ep = EndpointHandle(reply[0]);
    let devmgr_ep = EndpointHandle(reply[1]);
    puts("blk: bootstrapped\n");

    // Probe virtio-mmio devices to find a block device (DeviceID == 2).
    let mut skip: u64 = 0;
    let mmio_addr = loop {
        let probe = match sys_call_ret4(devmgr_ep, reply_obj,
            CMD_PROBE_DEVICE, VIRTIO_MMIO_HASH, skip, 0)
        {
            Ok(r) => r,
            Err(_) => { puts("blk: probe FAILED\n"); halt(); }
        };
        let status = probe[0];
        let addr = probe[1];
        let device_id = probe[3];
        if status == PROBE_END {
            puts("blk: no virtio-blk device found\n");
            halt();
        }
        if status != PROBE_OK || device_id != DEVICE_ID_BLOCK as u64 {
            skip += 1;
            continue;
        }
        break addr;
    };
    puts("blk: found virtio-blk device\n");

    // Claim the device by address and get a typed MMIO handle back.
    // claim_typed contains the single unsafe block that wraps the raw
    // VA in a MappedRegs<VirtioMmio>; the driver crate is unsafe-free.
    let claimed = match claim_typed::<VirtioMmio>(devmgr_ep, reply_obj, mmio_addr) {
        Ok(c) => c,
        Err(_) => { puts("blk: claim FAILED\n"); halt(); }
    };
    let mmio = claimed.regs;
    let irq_intid = claimed.irq_intid as u64;
    puts("blk: claimed device, intid=");
    put_decimal(irq_intid);
    puts("\n");

    // Verify magic.
    let magic = mmio.regs().read_magic_value();
    if magic != VIRTIO_MMIO_MAGIC_VALUE {
        puts("blk: bad magic\n");
        halt();
    }

    // VirtIO initialization sequence (spec 3.1.1).
    let regs = mmio.regs();

    // 1. Reset
    regs.set_status(Status::empty());
    // 2. Acknowledge
    regs.set_status(Status::ACKNOWLEDGE);
    // 3. Driver
    regs.set_status(Status::ACKNOWLEDGE | Status::DRIVER);

    // 4. Read features (windowed 32-bit protocol)
    regs.write_device_features_sel(0);
    let feat_low = regs.read_device_features();
    regs.write_device_features_sel(1);
    let feat_high = regs.read_device_features();

    // 5. Negotiate: require VERSION_1, no optional features.
    let mut neg = FeatureNegotiation::from_device(feat_low, feat_high);
    let (drv_low, drv_high) = neg.accept(BLK_DRIVER_WANTED);
    if !neg.is_modern() {
        puts("blk: device does not support VERSION_1\n");
        regs.set_status(Status::FAILED);
        halt();
    }
    regs.write_driver_features_sel(0);
    regs.write_driver_features(drv_low);
    regs.write_driver_features_sel(1);
    regs.write_driver_features(drv_high);

    // 6. Features OK
    regs.set_status(Status::ACKNOWLEDGE | Status::DRIVER | Status::FEATURES_OK);
    if !regs.status().contains(Status::FEATURES_OK) {
        puts("blk: FEATURES_OK rejected by device\n");
        regs.set_status(Status::FAILED);
        halt();
    }

    // 7. Setup virtqueue 0
    regs.write_queue_sel(0);
    let queue_max = regs.read_queue_num_max();
    if queue_max == 0 {
        puts("blk: queue not available\n");
        halt();
    }
    let qs = if (QUEUE_SIZE as u32) <= queue_max { QUEUE_SIZE } else { queue_max as u16 };

    // Allocate the virtqueue backing region as a typed DmaPage. The
    // page is zeroed by alloc_contiguous() and Virtqueue::new wires
    // up the typed cell/slice handles over it.
    let vq_total = virtqueue_layout(qs).total_size as u64;
    let vq_pages = (vq_total + PAGE_SIZE - 1) / PAGE_SIZE;
    let vq_dma = match DmaPage::alloc_contiguous(vq_pages) {
        Ok(p) => p,
        Err(_) => { puts("blk: alloc vq pages FAILED\n"); halt(); }
    };

    let vq = Virtqueue::new(vq_dma, qs);

    // Program queue registers.
    regs.write_queue_num(qs as u32);
    regs.write_queue_desc_low(vq.desc_phys() as u32);
    regs.write_queue_desc_high((vq.desc_phys() >> 32) as u32);
    regs.write_queue_driver_low(vq.avail_phys() as u32);
    regs.write_queue_driver_high((vq.avail_phys() >> 32) as u32);
    regs.write_queue_device_low(vq.used_phys() as u32);
    regs.write_queue_device_high((vq.used_phys() >> 32) as u32);
    regs.write_queue_ready(1);

    // 8. Driver OK
    regs.set_status(
        Status::ACKNOWLEDGE | Status::DRIVER | Status::FEATURES_OK | Status::DRIVER_OK
    );

    // Read block config: capacity is a le64 split across two RO u32s.
    let capacity = regs.read_blk_capacity_low() as u64
        | ((regs.read_blk_capacity_high() as u64) << 32);

    puts("blk: initialized, capacity=");
    put_decimal(capacity);
    puts(" sectors\n");

    // Allocate a page for request headers + status bytes (reused per I/O).
    // Typed views are constructed per-use inside do_io so they stay
    // lifetime-bound to `req_page`.
    let req_page = match DmaPage::alloc() {
        Ok(p) => p,
        Err(_) => { puts("blk: alloc req page FAILED\n"); halt(); }
    };

    // Bind IRQ.
    let irq_notif = match sys_alloc_pages(1).and_then(sys_create_notification) {
        Ok(h) => h,
        Err(_) => { puts("blk: create irq notif FAILED\n"); halt(); }
    };
    let bind_err = sys_bind_irq_flags(irq_intid, irq_notif, IRQ_FLAG_EDGE);
    if !bind_err.is_ok() {
        puts("blk: bind IRQ FAILED\n");
        halt();
    }
    puts("blk: IRQ bound\n");

    let mut engine = VirtioBlkEngine {
        mmio,
        vq,
        capacity,
        req_page,
        irq_notif,
        irq_threshold: 1,
        dma_buffers: [DMA_BUFFER_EMPTY; MAX_DMA_BUFFERS],
        dma_count: 0,
    };

    // Self-test: read sector 0 and print the first 16 bytes.
    let test_buf = engine.alloc_buffer(1).expect("blk: selftest alloc");
    let test_page = match DmaPage::map_existing(test_buf, 1) {
        Ok(p) => p,
        Err(_) => { puts("blk: selftest map FAILED\n"); halt(); }
    };
    test_page.zero();
    match engine.read(0, 1, test_buf) {
        Ok(()) => {
            // Emit the whole hex dump atomically via a stack buffer
            // so concurrent driver output can't interleave between
            // bytes. 16 bytes × ("XX" + space) - 1 trailing space
            // = 47 chars, plus the leading prefix and trailing "]\n".
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
            // test_slice (lifetime-bound to &test_page) goes out of
            // scope here, freeing &test_page so we can move it into
            // unmap() below.
        }
        Err(_) => {
            puts("blk: selftest read FAILED\n");
        }
    }
    let _ = test_page.unmap();
    engine.free_buffer(test_buf);

    puts("blk: serving\n");
    run_block_server(&mut engine, server_ep);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Terminate the driver process. EL0 wfi-loops keep the thread in
/// `Running` state from the scheduler's POV — they don't block,
/// they spin a tick-period each iteration after the next IRQ wakes
/// the CPU. Use sys_exit so the scheduler removes us from rotation.
fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("blk: PANIC\n");
    halt();
}
