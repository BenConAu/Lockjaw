#!/bin/bash
# QEMU integration tests for Lockjaw.
# Boots the kernel, captures serial output, and asserts expected strings.
set -e

TIMEOUT=30
QEMU="qemu-system-aarch64"
QEMU_FLAGS="-machine virt,gic-version=3 -cpu cortex-a53 -nographic"
KERNEL="target/aarch64-unknown-none/debug/lockjaw"

echo "=== Lockjaw QEMU Integration Tests ==="
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

echo "Phase 1 — Boot:"
assert_contains "Lockjaw Microkernel" "Boot banner printed"
assert_contains "Page allocator:" "Page allocator initialized"

echo "Phase 2 — Memory Management:"
assert_contains "MMU enabled" "MMU enabled with identity map"
assert_contains "Higher-half active" "Higher-half kernel mapping"
assert_contains "Guard page active" "Guard page unmapped"
assert_contains "Stack canary intact" "Stack canary written and verified"

echo "Phase 3 — Exceptions and Interrupts:"
assert_contains "Exception vectors installed" "Exception vector table at VBAR"
assert_contains "GIC initialized" "GICv3 interrupt controller"
assert_contains "Timer armed" "Virtual timer configured"
assert_contains "ticks received" "Timer interrupts firing"

echo "Phase 4 — Object Model:"
assert_contains "HandleTable" "HandleTable created via create-info pattern"
assert_contains "Inserted handle" "Handle insert works"
assert_contains "InsufficientRights" "Rights check rejects unauthorized access"
assert_contains "InvalidHandle" "Removed handle becomes invalid"

echo "Phase 5 — Threads:"
assert_contains "Scheduler started" "Round-robin scheduler running"

echo "Phase 6 — Syscalls:"
assert_contains "Dropping to EL0" "EL1 to EL0 transition"

echo "Phase 7 — IPC:"
assert_contains "Endpoint created" "Endpoint object created"
assert_contains "IPC BENCHMARK" "IPC benchmark completed"
assert_contains "call(" "Call/reply pattern working"

echo "Phase 8 — Userspace Processes:"
assert_contains "Loading init process" "Init ELF loading started"
assert_contains "Entry point: 0x400000" "ELF entry point parsed"
assert_contains "Address space created" "Per-process page tables allocated"
assert_contains "Hello from userspace init" "Init process running from ELF"
assert_contains "alloc_pages(1) OK" "sys_alloc_pages works from userspace"
assert_contains "map_pages OK" "sys_map_pages works from userspace"
assert_contains "mapped memory read/write OK" "Mapped memory accessible from userspace"
assert_contains "spawned OK" "Init spawned child via sys_create_process"
assert_contains "Hello from child process" "Child process running in own address space"
assert_contains "child: alive" "Child process scheduled and printing"

echo
echo "=== Results: $PASSED passed, $FAILED failed ==="

if [ $FAILED -gt 0 ]; then
    echo "INTEGRATION TESTS FAILED"
    exit 1
fi
echo "All integration tests passed."
