#![no_std]
#![no_main]

mod arch;

use arch::aarch64::uart::Uart;

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    let uart = Uart::new();
    uart.puts("Hello from kernel\n");

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
