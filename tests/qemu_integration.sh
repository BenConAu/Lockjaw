#!/bin/bash
# QEMU integration tests for Lockjaw.
# Boots the kernel, captures serial output, and asserts expected strings.
set -e

TIMEOUT=60
QEMU="qemu-system-aarch64"
GIC_VERSION="${GIC_VERSION:-3}"
# Two UARTs muxed onto stdio: UART0 for kernel debug, UART1 for the
# userspace UART driver to claim. Single `-nographic` only exposes one,
# leaving the driver with nothing to claim and breaking the
# uart-driver-related assertions further down.
QEMU_FLAGS="-machine virt,gic-version=${GIC_VERSION} -cpu cortex-a53 -display none \
    -chardev stdio,mux=on,id=char0 -mon chardev=char0,mode=readline \
    -serial chardev:char0 -serial chardev:char0 \
    -global virtio-mmio.force-legacy=false \
    -drive file=test.img,format=raw,if=none,id=blk0 \
    -device virtio-blk-device,drive=blk0"
KERNEL="target/aarch64-unknown-none/debug/lockjaw"

if [ ! -f test.img ]; then
    echo "ERROR: test.img not found. Run 'make test-img' first (or 'make test')." >&2
    exit 1
fi

echo "=== Lockjaw QEMU Integration Tests (GICv${GIC_VERSION}) ==="
echo "Booting kernel with ${TIMEOUT}s timeout..."
echo

OUTPUT=$(timeout $TIMEOUT $QEMU $QEMU_FLAGS -kernel $KERNEL 2>&1 || true)

PASSED=0
FAILED=0

assert_contains() {
    if echo "$OUTPUT" | grep -q "$1"; then
        echo "  PASS: $2"
        PASSED=$((PASSED + 1))
    else
        echo "  FAIL: $2 (expected to find: '$1')"
        FAILED=$((FAILED + 1))
    fi
}

assert_not_contains() {
    if echo "$OUTPUT" | grep -q "$1"; then
        echo "  FAIL: $2 (unexpected: '$1')"
        FAILED=$((FAILED + 1))
    else
        echo "  PASS: $2"
        PASSED=$((PASSED + 1))
    fi
}

echo "Phase 1 — Boot:"
assert_contains "Lockjaw Microkernel" "Boot banner printed"
assert_contains "Page allocator:" "Page allocator initialized"

echo "Phase 2 — Memory Management:"
assert_contains "MMU enabled" "MMU enabled with identity map"
assert_contains "Higher-half active" "Higher-half kernel mapping"
assert_contains "Guard pages active" "Guard pages unmapped"
assert_contains "Stack canary intact" "Stack canary written and verified"

echo "Phase 3 — Exceptions and Interrupts:"
assert_contains "Exception vectors installed" "Exception vector table at VBAR"
assert_contains "GIC initialized" "GIC interrupt controller initialized"
assert_contains "GICv${GIC_VERSION} distributor" "Correct GIC version detected"
assert_contains "Timer armed" "Virtual timer configured"
assert_contains "ticks received" "Timer interrupts firing"

echo "Phase 4 — Object Model:"
assert_contains "HandleTable" "HandleTable created via create-info pattern"
assert_contains "Inserted handle" "Handle insert works"
assert_contains "InsufficientRights" "Rights check rejects unauthorized access"
assert_contains "InvalidHandle" "Removed handle becomes invalid"
assert_contains "Process lifecycle test passed" "Process lifecycle 2-thread test"

echo "Phase 5 — Threads:"
assert_contains "Scheduler started" "Round-robin scheduler running"

echo "Phase 6 — Syscalls:"
assert_contains "Dropping to EL0" "EL1 to EL0 transition"

echo "Phase 7 — IPC:"
assert_contains "Endpoint created" "Endpoint object created"
assert_contains "IPC BENCHMARK" "IPC benchmark completed"
assert_contains "call(" "Call/reply pattern working"
assert_contains "SCHED-KERNEL-PHASE.*TTBR0 writes: 0" "No TTBR0 writes during kernel-only phase"

echo "Phase 8 — Userspace Processes:"
assert_contains "Loading init process" "Init ELF loading started"
assert_contains "Entry point: 0x400000" "ELF entry point parsed"
assert_contains "Address space created" "Per-process page tables allocated"
assert_contains "Hello from userspace init" "Init process running from ELF"
assert_contains "alloc_pages(1) OK" "sys_alloc_pages works from userspace"
assert_contains "map_pages OK" "sys_map_pages works from userspace"
assert_contains "mapped memory read/write OK" "Mapped memory accessible from userspace"
assert_contains "sys_export_handle validation OK" "sys_export_handle validates (no caller)"
assert_contains "DTB PageSet OK" "sys_get_boot_info returns valid DTB"
assert_contains "spawned OK" "Init spawned child via sys_create_process"
assert_contains "Hello from child process" "Child process running in own address space"
assert_contains "child: alive" "Child process scheduled and printing"
assert_not_contains "token ZERO" "Caller token is nonzero"
assert_not_contains "bootstrap receive FAILED" "Hello bootstrap receive succeeded"
assert_contains "caller token OK" "Caller token delivered via IPC"
assert_contains "\[BOOTSTRAP\] hello" "Init-hello bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] devmgr" "Init-devmgr bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] uart" "Init-uart bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] ramfb" "Init-ramfb bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] blk" "Init-blk bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] display-test" "Init-display-test bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] posix-server" "Init-posix-server bootstrap IPC completed"

echo "Phase 9 — Thread Exit:"
assert_contains "\[EXIT\] Thread" "Thread cleanup ran (finish_exit)"
assert_contains "pages freed" "Thread exit freed resources"

echo "Phase 10 — Thread Creation:"
assert_contains "\[THREAD-TEST\] child wrote marker" "sys_create_thread works (shared memory + exit)"

echo "Phase 10 — Device Manager:"
assert_contains "devmgr: parsed DTB" "Device manager parsed DTB"
assert_contains "devmgr: claimed device at 0x9040000" "Device manager claim-by-addr (UART)"
assert_contains "devmgr: claimed device at 0x9020000" "Device manager claim-by-addr (fw_cfg)"
assert_contains "devmgr: serving" "Device manager IPC loop running"

echo "Phase 10 — UART Driver:"
assert_contains "uart-driver: claimed PL011" "UART driver claimed PL011 from devmgr"
assert_contains "uart-driver: MMIO mapped" "UART driver mapped MMIO PageSet"
assert_contains "uart-driver: IRQ bound" "UART driver bound IRQ → notification"
assert_contains "uart-driver: server ready" "UART driver server loop entered"

echo "Phase 10 — ramfb Display Driver:"
assert_contains "ramfb: claimed fw_cfg" "ramfb claimed fw_cfg from devmgr"
assert_contains "ramfb: fw_cfg mapped" "ramfb mapped fw_cfg MMIO"

echo "Phase 10 — Display Test Client:"
assert_contains "\[DISPLAY-TEST\] starting" "Display test client started"
assert_contains "\[DISPLAY-TEST\] bootstrapped" "Display test client bootstrapped"

echo "Phase 14 — VirtIO Block Driver (with disk):"
assert_contains "blk: starting" "blk driver started"
assert_contains "blk: bootstrapped" "blk driver completed bootstrap"
assert_contains "blk: IRQ bound" "blk driver bound IRQ → notification"
# A real virtio-blk-device is attached (test.img, 64 MiB FAT32).
# Driver probes, allocates a DMA buffer, reads sector 0, prints the
# first 16 bytes. mformat-built FAT32 starts with "EB 58 90" (the
# JMP+NOP boot stub); we just match the prefix to prove the IPC +
# virtqueue + IRQ wait round-trip worked.
assert_contains "blk: selftest read OK, sector 0 = \[eb 58 90" "blk driver read sector 0 from disk"
assert_contains "blk: serving" "blk driver entered server loop after selftest"
assert_not_contains "blk: no virtio-blk device found" "blk driver did NOT take the no-device path"

echo "Phase 16 — POSIX Personality (Phase 0):"
assert_contains "posix-server: starting" "posix-server started"
assert_contains "posix-server: posix-hello spawned OK" "posix-server spawned musl child"
assert_contains "posix-server: POSIX_INIT OK" "POSIX_INIT bootstrap handshake completed"
assert_contains "hello, lockjaw" "musl puts() reached kernel UART (Phase 0 gate)"
assert_contains "posix-server: child exit" "posix-server saw child exit_group"
assert_contains "posix-server: done" "posix-server dispatch loop terminated cleanly"

# Fail explicitly if the thread test reported failure
if echo "$OUTPUT" | grep -q "\[THREAD-TEST\] FAILED"; then
    echo "  FAIL: Thread test reported failure"
    FAILED=$((FAILED + 1))
fi

echo
echo "=== Results: $PASSED passed, $FAILED failed ==="

if [ $FAILED -gt 0 ]; then
    echo "INTEGRATION TESTS FAILED"
    echo
    echo "--- Full QEMU output (for debugging missing assertions) ---"
    echo "$OUTPUT"
    echo "--- End of QEMU output ---"
    exit 1
fi
echo "All integration tests passed."
