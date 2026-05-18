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

echo "Phase 7 — Pre-EL0 boot:"
assert_contains "SCHED-KERNEL-PHASE.*TTBR0 writes: 0" "No TTBR0 writes during kernel-only phase"

echo "Phase 8 — Userspace Processes:"
assert_contains "Loading init process" "Init ELF loading started"
assert_contains "Entry point: 0x400000" "ELF entry point parsed"
assert_contains "Address space created" "Per-process page tables allocated"
assert_contains "Hello from userspace init" "Init process running from ELF"
# S1 EL0-time gate: CNTKCTL_EL1.EL0VCTEN+EL0PCTEN allow EL0 to read
# CNTVCT_EL0 / CNTFRQ_EL0 with bare `mrs`. The trap that would fire
# if the kernel hadn't set those bits is synchronous, so the boot
# would die before printing this line. QEMU virt with cortex-a53
# reports CNTFRQ_EL0 = 62.5 MHz; pin the value so a future build
# that loses CNTKCTL wiring (or that QEMU silently changes the
# default frequency) fails loudly here, not by symptom downstream.
assert_contains "init: EL0 CNTFRQ=62500000 CNTVCT=" "EL0 mrs of CNTVCT_EL0/CNTFRQ_EL0 succeeds (no trap)"
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
assert_contains "\[BOOTSTRAP\] fat32" "Init-fat32 bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] fat32-test" "Init-fat32-test bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] posix-server" "Init-posix-server bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] cprman" "Init-cprman bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] clock-test" "Init-clock-test bootstrap IPC completed"
assert_contains "\[BOOTSTRAP\] emmc2" "Init-emmc2 bootstrap IPC completed"
# M0c clock-provider arbitration: device-manager validates incoming
# CMD_GET_CLOCK_HANDLE requests against its registry of clock providers
# (built from the DTB scan at startup; today only bcm2711-cprman is
# recognised). On QEMU virt the cprman device is absent, so any request
# -- including the deliberately-bogus placeholder phandle the test client
# sends -- must come back as NoProvider. This locks down the validation
# gate; the SET_RATE forwarding path through the proxy is exercised on
# Pi 4B from M1 onward when emmc2-driver acquires a real handle.
assert_contains "devmgr: no cprman in DTB; clock requests will return NoProvider" \
    "M0c device-manager has no cprman provider on QEMU virt"
assert_contains "\[CLOCK-TEST\] CMD_GET_CLOCK_HANDLE refused unregistered phandle (expected on QEMU)" \
    "M0c device-manager refuses CMD_GET_CLOCK_HANDLE for unregistered controller_phandle"
assert_not_contains "\[CLOCK-TEST\] BUG:" \
    "M0c device-manager did not silently accept the bogus controller_phandle"
# M1: emmc2-driver claims bcm2711-emmc2 on Pi 4B; on QEMU virt the
# device is absent so it exits cleanly with a log line. We assert the
# clean-exit line rather than the Pi success line (which would never
# appear in QEMU output).
assert_contains "\[EMMC2:INIT\] no bcm2711-emmc2 device on this platform (QEMU)" \
    "M1 emmc2-driver exits cleanly when bcm2711-emmc2 absent (QEMU)"

# S4 sleep primitive: sleep-test asks the kernel for a 50ms sleep via
# sys_wait_any(deadline) and prints elapsed measured by monotonic_now()
# (mrs CNTVCT_EL0 from EL0 — gated by S1's CNTKCTL_EL1 setup).
# Tolerance window [50ms, 200ms]: lower bound = the deadline floor;
# upper bound is loose because the QEMU host is single-CPU and the
# sleep coincides with concurrent driver startup (uart, devmgr,
# posix-server, cprman, ramfb, clock-test, emmc2). All those threads
# fair-share the CPU while sleep-test is Blocked; sleep-test only
# resumes once the round-robin reaches it after wake_expired_deadlines
# flips its state Ready. This test asserts the wake mechanism FIRES,
# not exact latency — a real perf SLO belongs in a workload-specific
# benchmark, not in a generic boot smoke test.
assert_contains "\[BOOTSTRAP\] sleep-test" "Init-sleep-test bootstrap IPC completed"
assert_contains "\[SLEEP-TEST\] elapsed within tolerance" \
    "S4 sleep_for(50ms) elapsed in [50ms, 200ms]"
assert_not_contains "\[SLEEP-TEST\] elapsed OUT OF TOLERANCE" \
    "S4 sleep deadline did not over-shoot or under-shoot"

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

echo "Phase 14.5 — Partition Manager:"
# Bare FAT32 test.img: parse_disk returns BareFat, start_lba=0, pass-through.
assert_contains "partmgr: starting" "partition-manager started"
assert_contains "partmgr: bootstrapped" "partition-manager completed bootstrap"
assert_contains "partmgr: bare FAT32 disk" "partition-manager detected bare FAT32 layout"
assert_contains "partmgr: serving" "partition-manager entered server loop"

echo "Phase 15 — FAT32 Filesystem Server (mount):"
assert_contains "fat32: starting" "fat32-server started"
assert_contains "fat32: bootstrapped" "fat32-server completed bootstrap"
# Mounts the disk: reads sector 0 via the block driver, parses the BPB.
# Don't pin formatter-dependent geometry (cluster size depends on
# mformat's heuristic for the 64 MiB image and could shift across
# mtools versions). Two robust facts:
#   - "fat32: mounted" only appears if parse_bpb succeeded
#   - root cluster is stable: mformat follows the FAT32 spec recommendation
#     of 2 (the first usable data cluster) for every volume it produces
assert_contains "fat32: mounted" "fat32-server mounted disk and parsed BPB"
assert_contains "root_cluster=2" "fat32-server identified root cluster as 2"

echo "Phase 15 — FAT32 Verification Client (open + read end-to-end):"
assert_contains "\[FAT32-TEST\] starting" "fat32-test client started"
assert_contains "\[FAT32-TEST\] bootstrapped" "fat32-test client bootstrapped"
assert_contains "\[FAT32-TEST\] opened /HELLO.TXT" "fat32-test opened /HELLO.TXT"
# HELLO.TXT contains "hello from fat32\n" (17 bytes; mcopy added it
# during test.img generation). The read goes through:
#   fat32-test -> FsClient (FS_OPEN) -> fat32-server -> BlockClient
#   -> virtio-blk-driver -> QEMU -> test.img
# This single line proves the entire stack works end to end.
assert_contains "\[FAT32-TEST\] read 17 bytes: hello from fat32" "fat32-test read HELLO.TXT contents via FS IPC"
assert_contains "\[FAT32-TEST\] done" "fat32-test completed cleanly"

echo "Phase 16 — POSIX Personality (Phase 0 + Phase 1):"
assert_contains "posix-server: starting" "posix-server started"
assert_contains "posix-server: posix-hello spawned OK" "posix-server spawned musl child"
assert_contains "posix-server: POSIX_INIT OK" "POSIX_INIT bootstrap handshake completed"
assert_contains "hello, lockjaw" "musl puts() reached kernel UART (Phase 0 gate)"
# Phase 1 gate via the musl path: hello.c does fopen("/HELLO.TXT") +
# fread + printf. Exercises the full stack: musl libc -> shim -> posix-server
# -> FsClient -> fat32-server -> BlockClient -> virtio-blk -> QEMU disk.
assert_contains "posix-hello: hello from fat32" "musl read FAT32 file via posix-server (Phase 1 gate)"
assert_contains "posix-hello: malloc 1MB ok" "musl malloc(1MB) via mmap (Phase 2.3 gate)"
assert_contains "posix-hello: malloc 8MB ok" "musl malloc(8MB) via single-PageSet mmap (Phase 2.4 gate)"
assert_contains "posix-server: child exit" "posix-server saw child exit_group"
assert_contains "posix-server: done" "posix-server dispatch loop terminated cleanly"

echo "Phase 17 — Handle revocation (commit 2 lockdown):"
# Diagnostic format: "revoke OK: header=N procs=N slots=N maps=N"
# Emitted from consume_pageset_apply on every kernel-object create
# (sys_create_endpoint/notification/reply) and every sys_create_process
# segment consume. Asserting the prefix proves the revoke walker actually
# runs on every consume — a baseline silent kprintln-removal would let a
# regression pass make test otherwise.
assert_contains "revoke OK:" "Revocation walker fires on consume_pageset_apply"

# Multi-process walk: assert that at least one consume sees procs >= 2.
# The exact count is intentionally not pinned — it depends on which
# threads are registered in scheduler::threads at the moment of
# consume_pageset_apply, which is sensitive to boot order and (for
# sys_create_process) does NOT include the child being spawned (the
# child is added to the scheduler after the apply phase completes).
# A regex over [2-9] or two-digit counts catches any nontrivial walk
# without locking in today's specific number.
if echo "$OUTPUT" | grep -qE "revoke OK:.*procs=([2-9]|[1-9][0-9]+)"; then
    echo "  PASS: Revoke walker visits multiple processes (procs >= 2)"
    PASSED=$((PASSED + 1))
else
    echo "  FAIL: Revoke walker never reached procs >= 2"
    FAILED=$((FAILED + 1))
fi

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
