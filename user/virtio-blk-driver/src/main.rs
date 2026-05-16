#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;
use lockjaw_userlib::handle::PageSetGuard;
use lockjaw_userlib::block::{BlockEngine, BlockInfo, BlockError, run_block_server};
use lockjaw_userlib::virtqueue::{Virtqueue, mmio_read32, mmio_write32};
use lockjaw_types::virtio::*;
use lockjaw_types::device::{VIRTIO_MMIO_HASH, CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR,
                            PROBE_OK, PROBE_END};

// ---------------------------------------------------------------------------
// VirtIO block engine
// ---------------------------------------------------------------------------

/// Maximum number of tracked DMA buffers.
const MAX_DMA_BUFFERS: usize = 8;

/// Virtqueue size to use (must be <= device's QUEUE_NUM_MAX).
const QUEUE_SIZE: u16 = 128;

struct VirtioBlkEngine {
    /// MMIO base virtual address.
    mmio_va: u64,
    /// Virtqueue for block I/O requests.
    vq: Virtqueue,
    /// Block device capacity in sectors.
    capacity: u64,
    /// Physical address of the request header page (reused across I/Os).
    req_header_pa: u64,
    /// VA of the request header page.
    req_header_va: u64,
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
        }
    }

    fn alloc_buffer(&mut self, sector_count: u64) -> Result<PageSetHandle, BlockError> {
        if self.dma_count >= MAX_DMA_BUFFERS {
            return Err(BlockError::AllocFailed);
        }
        let pages = (sector_count * 512 + PAGE_SIZE - 1) / PAGE_SIZE;
        // Guard closes the handle on drop if any subsequent step fails.
        let guard = PageSetGuard::new(
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
        unsafe {
            ptr::write_volatile(self.req_header_va as *mut VirtioBlkReqHeader,
                VirtioBlkReqHeader {
                    req_type,
                    reserved: 0,
                    sector,
                });
        }

        // Status byte lives after the header in the same page.
        let status_pa = self.req_header_pa + 16;
        let status_va = self.req_header_va + 16;
        // Clear status byte before submission.
        unsafe { ptr::write_volatile(status_va as *mut u8, 0xFF); }

        // Data descriptor flags: device-writable for reads, device-readable for writes.
        let data_flags = if req_type == VIRTIO_BLK_T_IN {
            VIRTQ_DESC_F_WRITE
        } else {
            0
        };

        // Allocate 3-descriptor chain: header + data + status.
        let head = self.vq.alloc_chain3(
            self.req_header_pa, 16, 0,              // header: device-readable
            data_pa, data_len, data_flags,           // data
            status_pa, 1, VIRTQ_DESC_F_WRITE,        // status: device-writable
        ).ok_or(BlockError::AllocFailed)?;

        // Submit and notify device.
        self.vq.submit(head);
        unsafe {
            mmio_write32(self.mmio_va, VIRTIO_MMIO_QUEUE_NOTIFY, 0);
        }

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
            // ACK the interrupt.
            unsafe {
                let status = mmio_read32(self.mmio_va, VIRTIO_MMIO_INTERRUPT_STATUS);
                mmio_write32(self.mmio_va, VIRTIO_MMIO_INTERRUPT_ACK, status);
            }
        }

        // Free the descriptor chain.
        self.vq.free_chain(head);

        // Check status byte.
        let status = unsafe { ptr::read_volatile(status_va as *const u8) };
        if status != VIRTIO_BLK_S_OK {
            return Err(BlockError::IoError);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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

    // Claim the device by address (TOCTOU-safe).
    let claim = match sys_call_ret4(devmgr_ep, reply_obj,
        CMD_CLAIM_BY_ADDR, mmio_addr, 0, 0)
    {
        Ok(r) => r,
        Err(_) => { puts("blk: claim FAILED\n"); halt(); }
    };
    if claim[0] != lockjaw_types::device::CLAIM_OK {
        puts("blk: claim rejected\n");
        halt();
    }
    let mmio_ps = PageSetHandle(claim[1]);
    let irq_intid = claim[2];
    puts("blk: claimed device, intid=");
    put_decimal(irq_intid);
    puts("\n");

    // Map MMIO page. Multiple virtio-mmio devices share a single 4K page
    // (each device is 512 bytes), so add the intra-page offset.
    let mmio_page_va = VMEM.alloc(1).expect("VA exhausted for MMIO");
    if !sys_map_pages(mmio_ps, mmio_page_va, MapMemoryAttribute::Device).is_ok() {
        puts("blk: map MMIO FAILED\n");
        halt();
    }
    let mmio_va = mmio_page_va + (mmio_addr & 0xFFF);

    // Verify magic.
    let magic = unsafe { mmio_read32(mmio_va, VIRTIO_MMIO_MAGIC) };
    if magic != VIRTIO_MMIO_MAGIC_VALUE {
        puts("blk: bad magic\n");
        halt();
    }

    // VirtIO initialization sequence (spec 3.1.1).
    unsafe {
        // 1. Reset
        mmio_write32(mmio_va, VIRTIO_MMIO_STATUS, 0);
        // 2. Acknowledge
        mmio_write32(mmio_va, VIRTIO_MMIO_STATUS, STATUS_ACKNOWLEDGE);
        // 3. Driver
        mmio_write32(mmio_va, VIRTIO_MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

        // 4. Read features (windowed 32-bit protocol)
        mmio_write32(mmio_va, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
        let feat_low = mmio_read32(mmio_va, VIRTIO_MMIO_DEVICE_FEATURES);
        mmio_write32(mmio_va, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1);
        let feat_high = mmio_read32(mmio_va, VIRTIO_MMIO_DEVICE_FEATURES);

        // 5. Negotiate: require VERSION_1, no optional features.
        let mut neg = FeatureNegotiation::from_device(feat_low, feat_high);
        let (drv_low, drv_high) = neg.accept(BLK_DRIVER_WANTED);
        if !neg.is_modern() {
            puts("blk: device does not support VERSION_1\n");
            mmio_write32(mmio_va, VIRTIO_MMIO_STATUS, STATUS_FAILED);
            halt();
        }
        mmio_write32(mmio_va, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
        mmio_write32(mmio_va, VIRTIO_MMIO_DRIVER_FEATURES, drv_low);
        mmio_write32(mmio_va, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
        mmio_write32(mmio_va, VIRTIO_MMIO_DRIVER_FEATURES, drv_high);

        // 6. Features OK
        mmio_write32(mmio_va, VIRTIO_MMIO_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        let status = mmio_read32(mmio_va, VIRTIO_MMIO_STATUS);
        if status & STATUS_FEATURES_OK == 0 {
            puts("blk: FEATURES_OK rejected by device\n");
            mmio_write32(mmio_va, VIRTIO_MMIO_STATUS, STATUS_FAILED);
            halt();
        }

        // 7. Setup virtqueue 0
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_SEL, 0);
        let queue_max = mmio_read32(mmio_va, VIRTIO_MMIO_QUEUE_NUM_MAX);
        if queue_max == 0 {
            puts("blk: queue not available\n");
            halt();
        }
        let qs = if (QUEUE_SIZE as u32) <= queue_max { QUEUE_SIZE } else { queue_max as u16 };

        // Allocate contiguous pages for virtqueue.
        let vq_layout = virtqueue_layout(qs);
        let vq_pages = ((vq_layout.total_size + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize) as u64;
        let vq_ps = sys_alloc_pages_contiguous(vq_pages).expect("blk: alloc vq pages");
        let vq_pa = sys_query_pageset_phys(vq_ps, 0).expect("blk: query vq phys");
        let vq_va = VMEM.alloc(vq_pages as usize).expect("VA exhausted for vq");
        if !sys_map_pages(vq_ps, vq_va, MapMemoryAttribute::Normal).is_ok() {
            puts("blk: map vq FAILED\n");
            halt();
        }
        // Zero the virtqueue pages.
        for i in 0..vq_pages {
            zero_page_at_va(vq_va + i * PAGE_SIZE);
        }

        let vq = Virtqueue::new(vq_va, vq_pa, qs);

        // Program queue registers.
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_NUM, qs as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DESC_LOW, vq.desc_phys() as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DESC_HIGH, (vq.desc_phys() >> 32) as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DRIVER_LOW, vq.avail_phys() as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DRIVER_HIGH, (vq.avail_phys() >> 32) as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DEVICE_LOW, vq.used_phys() as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_DEVICE_HIGH, (vq.used_phys() >> 32) as u32);
        mmio_write32(mmio_va, VIRTIO_MMIO_QUEUE_READY, 1);

        // 8. Driver OK
        mmio_write32(mmio_va, VIRTIO_MMIO_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

        // Read block config: capacity at config offset 0 (two 32-bit reads).
        let cap_lo = mmio_read32(mmio_va, VIRTIO_BLK_CFG_CAPACITY) as u64;
        let cap_hi = mmio_read32(mmio_va, VIRTIO_BLK_CFG_CAPACITY + 4) as u64;
        let capacity = cap_lo | (cap_hi << 32);

        puts("blk: initialized, capacity=");
        put_decimal(capacity);
        puts(" sectors\n");

        // Allocate a page for request headers + status bytes (reused).
        let req_ps = sys_alloc_pages(1).expect("blk: alloc req page");
        let req_pa = sys_query_pageset_phys(req_ps, 0).expect("blk: query req phys");
        let req_va = VMEM.alloc(1).expect("VA exhausted for req header");
        if !sys_map_pages(req_ps, req_va, MapMemoryAttribute::Normal).is_ok() {
            puts("blk: map req page FAILED\n");
            halt();
        }

        // Bind IRQ.
        let irq_notif = sys_alloc_pages(1).and_then(sys_create_notification)
            .expect("blk: create irq notif");
        if !sys_bind_irq_flags(irq_intid, irq_notif, IRQ_FLAG_EDGE).is_ok() {
            puts("blk: bind IRQ FAILED\n");
            halt();
        }
        puts("blk: IRQ bound\n");

        let mut engine = VirtioBlkEngine {
            mmio_va,
            vq,
            capacity,
            req_header_pa: req_pa,
            req_header_va: req_va,
            irq_notif,
            irq_threshold: 1,
            dma_buffers: [DMA_BUFFER_EMPTY; MAX_DMA_BUFFERS],
            dma_count: 0,
        };

        // Self-test: read sector 0 and print the first 16 bytes.
        let test_buf = engine.alloc_buffer(1).expect("blk: selftest alloc");
        let test_va = VMEM.alloc(1).expect("VA exhausted for selftest");
        if !sys_map_pages(test_buf, test_va, MapMemoryAttribute::Normal).is_ok() {
            puts("blk: selftest map FAILED\n");
            halt();
        }
        // Zero the buffer so we can distinguish read data from garbage.
        zero_page_at_va(test_va);
        match engine.read(0, 1, test_buf) {
            Ok(()) => {
                // Emit the whole hex dump atomically via a stack buffer
                // so concurrent driver output can't interleave between
                // bytes.  16 bytes × ("XX" + space) - 1 trailing space
                // = 47 chars, plus the leading prefix and trailing "]\n".
                let prefix = b"blk: selftest read OK, sector 0 = [";
                let mut buf = [0u8; 35 + 47 + 2];
                let mut len = 0;
                for &c in prefix { buf[len] = c; len += 1; }
                let data = test_va as *const u8;
                for i in 0..16 {
                    let b = core::ptr::read_volatile(data.add(i));
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
            Err(_) => {
                puts("blk: selftest read FAILED\n");
            }
        }
        let unmap_ok = sys_unmap_pages(test_buf, test_va).is_ok();
        engine.free_buffer(test_buf);
        if unmap_ok {
            VMEM.free(test_va, 1);
        }

        puts("blk: serving\n");
        run_block_server(&mut engine, server_ep);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

// put_decimal is imported from lockjaw_userlib (atomic emit).

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("blk: PANIC\n");
    halt();
}
