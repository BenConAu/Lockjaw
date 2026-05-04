#!/bin/bash
# Build the standalone POSIX hello test binary — a debug tool, not the
# default build target.
#
# Uses clang (any host clang with aarch64-elf target support — macOS Xcode
# CLT and most Linux clang installs qualify) and rustup's rust-lld linker
# (installed with `rustup target add aarch64-unknown-none`). No extra
# cross-toolchain required.
#
# Output: ./hello-standalone (statically-linked aarch64 ELF). The default
# build path uses musl via ../../musl-lockjaw/build.sh, which produces
# ./hello. This script exists to validate the IPC + personality server
# end-to-end without a musl toolchain dependency.
set -euo pipefail
cd "$(dirname "$0")"

# clang from PATH; allow override via $CLANG.
CLANG=${CLANG:-clang}
if ! command -v "$CLANG" >/dev/null 2>&1; then
    echo "ERROR: '$CLANG' not found in PATH. Set CLANG=/path/to/clang or install clang." >&2
    exit 1
fi

# rust-lld lives inside the active rust sysroot. Discover it via rustc.
SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | awk '/^host:/ {print $2}')
LD="${SYSROOT}/lib/rustlib/${HOST}/bin/rust-lld"
if [ ! -x "$LD" ]; then
    echo "ERROR: rust-lld not found at $LD." >&2
    echo "Install with: rustup component add llvm-tools-preview" >&2
    exit 1
fi

"$CLANG" -target aarch64-elf -ffreestanding -nostdlib -O2 \
    -c standalone.c -o standalone.o

"$LD" -flavor ld -m aarch64elf -T linker.ld standalone.o -o hello-standalone

rm -f standalone.o
echo "Built: $(pwd)/hello-standalone"
file hello-standalone
