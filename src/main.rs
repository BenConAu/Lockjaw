#![no_std]
#![no_main]

mod arch;

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
