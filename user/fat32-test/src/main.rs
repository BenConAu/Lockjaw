#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::fs::FsClient;

fn die(msg: &str) -> ! {
    puts(msg);
    sys_exit();
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("[FAT32-TEST] starting\n");

    let reply = sys_alloc_pages(1)
        .and_then(sys_create_reply)
        .unwrap_or_else(|_| die("[FAT32-TEST] create reply FAILED\n"));

    // Bootstrap: receive the fat32-server endpoint from init.
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => die("[FAT32-TEST] bootstrap FAILED\n"),
    };
    let fs_ep = EndpointHandle(bootstrap[0]);
    puts("[FAT32-TEST] bootstrapped\n");

    let fs = FsClient::new(fs_ep, reply);

    // Open /HELLO.TXT with a 1-page read buffer.
    let opened = match fs.open(b"/HELLO.TXT", 1) {
        Ok(o) => o,
        Err(_) => die("[FAT32-TEST] open FAILED\n"),
    };
    puts("[FAT32-TEST] opened /HELLO.TXT\n");

    // Map the per-handle buffer locally so we can read it.
    let buf_va = VMEM.alloc(1).expect("VA exhausted");
    if !sys_map_pages(opened.pageset, buf_va, MapMemoryAttribute::Normal).is_ok() {
        die("[FAT32-TEST] map FAILED\n");
    }

    // Read up to one page (the file is 17 bytes; this returns 17).
    let n = match fs.read(opened.handle, opened.buffer_size) {
        Ok(n) => n,
        Err(_) => die("[FAT32-TEST] read FAILED\n"),
    };

    // Emit the file contents atomically (so concurrent driver output
    // can't interleave between bytes).
    let prefix = b"[FAT32-TEST] read ";
    // 6 chars max for n (file size is bounded), 2 for ": ", file bytes
    // (capped at PAGE_SIZE), 1 for trailing '\n'.
    let mut out = [0u8; 24 + lockjaw_userlib::PAGE_SIZE as usize + 8];
    let mut len = 0;
    for &c in prefix { out[len] = c; len += 1; }
    // Decimal n
    if n == 0 {
        out[len] = b'0'; len += 1;
    } else {
        let mut tmp = [0u8; 10];
        let mut i = 0;
        let mut v = n;
        while v > 0 { tmp[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        for j in 0..i { out[len] = tmp[i - 1 - j]; len += 1; }
    }
    out[len] = b' '; len += 1;
    out[len] = b'b'; len += 1;
    out[len] = b'y'; len += 1;
    out[len] = b't'; len += 1;
    out[len] = b'e'; len += 1;
    out[len] = b's'; len += 1;
    out[len] = b':'; len += 1;
    out[len] = b' '; len += 1;
    // SAFETY: fat32-server wrote n bytes (n <= buffer_size = PAGE_SIZE)
    // into buf_va before replying.
    let bytes = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n as usize) };
    for &b in bytes { out[len] = b; len += 1; }
    if !bytes.last().map_or(false, |&b| b == b'\n') {
        out[len] = b'\n'; len += 1;
    }
    sys_debug_puts(&out[..len]);

    // Cleanup. Proof-token teardown: VA returns to VMEM only on
    // successful unmap (the safer failure mode vs. aliasing on reuse).
    let _ = fs.close(opened.handle);
    if let Ok(p) = unmap_pages_tracked(opened.pageset, buf_va, 1) {
        VMEM.free_unmapped(p);
    }
    let _ = sys_close_handle(opened.pageset);

    puts("[FAT32-TEST] done\n");
    sys_exit();
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("[FAT32-TEST] PANIC\n");
    sys_exit();
}
