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
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut uart = Uart::new();

    let _ = writeln!(uart, "\n!!! KERNEL PANIC !!!");
    if let Some(location) = info.location() {
        let _ = writeln!(uart, "  at {}:{}", location.file(), location.line());
    }
    let _ = writeln!(uart, "  {}", info.message());

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
