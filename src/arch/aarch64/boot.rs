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
    // Store DTB paddr to global (after BSS zeroing so we don't clobber it).
    bl      kmain                    // Call Rust entry point

    // --- Halt if kmain ever returns ---
.Lhalt:
    wfi                              // Wait for interrupt (low-power idle)
    b       .Lhalt                   // Loop forever

// ---------------------------------------------------------------------------
// Secondary CPU entry point — called by PSCI CPU_ON
// ---------------------------------------------------------------------------
// PSCI delivers x0 = context_id (we pass the cpu_id here).
// The secondary must: mask exceptions, drop to EL1 if at EL2, enable
// FP/NEON, set its per-CPU stack, and call secondary_main(cpu_id).
// No BSS zeroing — CPU 0 already did that.

.global _secondary_start
_secondary_start:
    msr     DAIFSet, #0xf            // Mask all exceptions

    // x0 = cpu_id (from PSCI context_id), preserve across EL2 drop
    mov     x19, x0                  // Save cpu_id in callee-saved register

    // --- EL2 → EL1 drop (same as primary) ---
    mrs     x1, CurrentEL            // Read CurrentEL
    lsr     x1, x1, #2              // EL number in bits [1:0]
    cmp     x1, #2                   // At EL2?
    b.ne    .Lsec_el1               // Already at EL1, skip

    mov     x1, #(1 << 31)          // HCR_EL2.RW = AArch64 EL1
    msr     HCR_EL2, x1
    mov     x1, #0x3c5              // SPSR_EL2: EL1h, DAIF masked
    msr     SPSR_EL2, x1
    adr     x1, .Lsec_el1
    msr     ELR_EL2, x1
    eret

.Lsec_el1:
    // --- Enable FP/NEON ---
    mov     x1, #(3 << 20)          // CPACR_EL1.FPEN = 0b11
    msr     CPACR_EL1, x1
    isb

    // --- Set per-CPU stack ---
    // Stack layout: __per_cpu_stacks + (cpu_id + 1) * 12288
    // Each CPU block: 4 KB guard + 8 KB stack = 12 KB = 12288 bytes.
    ldr     x1, =__per_cpu_stacks    // Base of all per-CPU stacks
    add     x2, x19, #1             // cpu_id + 1
    movz    x3, #12288               // Per-CPU block stride (4K guard + 8K stack)
    mul     x2, x2, x3              // (cpu_id + 1) * 12288
    add     sp, x1, x2              // SP = stack top for this CPU

    // --- Call Rust secondary_main(cpu_id) ---
    mov     x0, x19                  // Restore cpu_id as first argument
    bl      secondary_main           // Never returns

    // --- Halt if secondary_main ever returns ---
.Lsec_halt:
    wfi
    b       .Lsec_halt
"#
);
