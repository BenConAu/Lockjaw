#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::block::BlockClient;
use lockjaw_types::fat32::parse_bpb;

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("fat32: starting\n");

    // Allocate one Reply object for both bootstrap and outbound block IPCs.
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("fat32: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: receive fs_srv_ep (this server's own endpoint) and
    // blk_srv_ep (the block driver's server endpoint) from init.
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("fat32: bootstrap FAILED\n"); halt(); }
    };
    let fs_srv_ep = EndpointHandle(bootstrap[0]);
    let blk_srv_ep = EndpointHandle(bootstrap[1]);
    puts("fat32: bootstrapped\n");

    // Connect to the block driver as a typed client.
    let blk = BlockClient::new(blk_srv_ep, reply);

    // Allocate one sector-sized DMA buffer (1 sector = 512 bytes; the
    // block driver rounds up to a page).
    let buf = match blk.alloc_buffer(1) {
        Ok(b) => b,
        Err(_) => { puts("fat32: blk alloc_buffer FAILED\n"); halt(); }
    };

    // Map the buffer into our own address space so we can read the bytes.
    let buf_va = VMEM.alloc(1).expect("VA exhausted for boot sector buffer");
    if !sys_map_pages(buf.pageset, buf_va, 0).is_ok() {
        puts("fat32: blk buffer map FAILED\n");
        halt();
    }

    // Read sector 0 (the BPB).
    if blk.read(0, 1, buf.buffer_id).is_err() {
        puts("fat32: read sector 0 FAILED\n");
        halt();
    }

    // Parse the BPB. lockjaw_types::fat32 owns all validation
    // (host-tested); we just feed it the bytes.
    // SAFETY: buf_va points to a freshly mapped page; the block driver
    // just wrote sector 0 (512 bytes) into it. Reading the first 512
    // bytes as a fixed-size array is in-bounds.
    let sector0: &[u8; 512] = unsafe { &*(buf_va as *const [u8; 512]) };
    let geom = match parse_bpb(sector0) {
        Ok(g) => g,
        Err(_) => { puts("fat32: BPB parse FAILED\n"); halt(); }
    };

    puts("fat32: mounted, cluster_size=");
    put_decimal(geom.bytes_per_cluster() as u64);
    puts(" bytes, root_cluster=");
    put_decimal(geom.root_cluster as u64);
    puts(", clusters=");
    put_decimal(geom.cluster_count() as u64);
    puts("\n");

    // Stub IPC loop: the FS protocol arrives in Phase E. Until then,
    // any incoming request gets a sentinel "unsupported" reply so a
    // bring-up client doesn't block forever.
    loop {
        let _ = match sys_receive_ret4(fs_srv_ep) {
            Ok(m) => m,
            Err(_) => { puts("fat32: receive FAILED\n"); halt(); }
        };
        sys_reply(u64::MAX, 0, 0, 0);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("fat32: PANIC\n");
    halt();
}
