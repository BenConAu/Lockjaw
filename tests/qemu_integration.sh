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
assert_contains "GIC initialized" "GICv3 interrupt controller"
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

echo "Phase 9 — Thread Exit:"
assert_contains "\[EXIT\] Thread" "Thread cleanup ran (finish_exit)"
assert_contains "pages freed" "Thread exit freed resources"

echo "Phase 10 — Thread Creation:"
assert_contains "\[THREAD-TEST\] child wrote marker" "sys_create_thread works (shared memory + exit)"

# Fail explicitly if the thread test reported failure
if echo "$OUTPUT" | grep -q "\[THREAD-TEST\] FAILED"; then
    echo "  FAIL: Thread test reported failure"
    FAILED=$((FAILED + 1))
fi

echo
echo "=== Results: $PASSED passed, $FAILED failed ==="

if [ $FAILED -gt 0 ]; then
    echo "INTEGRATION TESTS FAILED"
    exit 1
fi
echo "All integration tests passed."
