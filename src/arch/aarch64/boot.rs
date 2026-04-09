use core::arch::global_asm;

global_asm!(
    r#"
.section .text._start, "ax"         // Place in .text._start section (executable)
.global _start                       // Export _start for the linker ENTRY directive

_start:
    msr     DAIFSet, #0xf            // Mask all exceptions (Debug, Async, IRQ, FIQ)

    // --- Determine current exception level ---
    mrs     x0, CurrentEL            // Read CurrentEL register
    lsr     x0, x0, #2              // Shift right to get EL number in bits [1:0]
    cmp     x0, #2                   // Are we at EL2?
    b.ne    .Lat_el1                 // If not EL2, skip the drop — already at EL1

    // --- Drop from EL2 to EL1 ---
    mov     x0, #(1 << 31)          // HCR_EL2.RW bit: EL1 executes in AArch64 mode
    msr     HCR_EL2, x0             // Write Hypervisor Configuration Register

    mov     x0, #0x3c5              // SPSR_EL2: EL1h (SP_EL1), DAIF bits all masked
    msr     SPSR_EL2, x0            // Set Saved Program Status Register for EL2

    adr     x0, .Lat_el1            // Load address of EL1 entry point
    msr     ELR_EL2, x0             // Set Exception Link Register — where eret jumps to

    eret                             // Return from EL2 → EL1 at .Lat_el1

.Lat_el1:
    // --- Enable FP/NEON (the compiler may generate SIMD instructions) ---
    mov     x0, #(3 << 20)          // CPACR_EL1.FPEN = 0b11: no FP/NEON trapping
    msr     CPACR_EL1, x0           // Write Coprocessor Access Control Register
    isb                              // Ensure CPACR is active before any FP use

    // --- Set up the kernel stack ---
    ldr     x0, =__stack_top         // Load stack top address from linker symbol
    mov     sp, x0                   // Initialize stack pointer (grows downward)

    // --- Zero the BSS section ---
    ldr     x0, =__bss_start         // x0 = start of BSS region
    ldr     x1, =__bss_end           // x1 = end of BSS region
.Lbss_loop:
    cmp     x0, x1                   // Have we reached the end?
    b.ge    .Lbss_done               // If so, stop
    str     xzr, [x0], #8           // Store zero (8 bytes), advance pointer
    b       .Lbss_loop               // Repeat

.Lbss_done:
    bl      kmain                    // Call Rust entry point

    // --- Halt if kmain ever returns ---
.Lhalt:
    wfi                              // Wait for interrupt (low-power idle)
    b       .Lhalt                   // Loop forever
"#
);
