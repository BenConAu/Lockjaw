#!/bin/bash
#
# Build patched musl and compile the POSIX hello binary for Lockjaw.
#
# Prerequisites:
#   brew install musl-cross   (provides aarch64-linux-musl-gcc)
#
# What this script does:
#   1. Clone musl (if not already present)
#   2. Apply Lockjaw patches (crt_arch.h, syscall_arch.h)
#   3. Add shim.c to the musl source tree
#   4. Build musl as a static library
#   5. Compile hello.c against patched musl → user/posix-hello/hello

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MUSL_VER="1.2.5"
MUSL_DIR="$SCRIPT_DIR/musl-$MUSL_VER"
CROSS=aarch64-linux-musl

# Check for cross-compiler
if ! command -v ${CROSS}-gcc &>/dev/null; then
    echo "ERROR: ${CROSS}-gcc not found."
    echo "Install with: brew install filosottile/musl-cross/musl-cross"
    exit 1
fi

# 1. Download musl source if not present
if [ ! -d "$MUSL_DIR" ]; then
    echo "==> Downloading musl $MUSL_VER..."
    cd "$SCRIPT_DIR"
    curl -LO "https://musl.libc.org/releases/musl-${MUSL_VER}.tar.gz"
    tar xzf "musl-${MUSL_VER}.tar.gz"
    rm "musl-${MUSL_VER}.tar.gz"
fi

# 2. Apply Lockjaw patches (preserve mtime — `cp -p` so unchanged inputs
#    don't trigger an unnecessary musl rebuild on the next invocation).
echo "==> Applying Lockjaw patches..."
cp -p "$SCRIPT_DIR/patches/crt_arch.h"     "$MUSL_DIR/arch/aarch64/crt_arch.h"
cp -p "$SCRIPT_DIR/patches/syscall_arch.h" "$MUSL_DIR/arch/aarch64/syscall_arch.h"

# 3. Add shim.c to the musl tree
mkdir -p "$MUSL_DIR/src/lockjaw"
cp -p "$SCRIPT_DIR/src/shim.c" "$MUSL_DIR/src/lockjaw/shim.c"

# 4. Build musl. Incremental: only reconfigure+rebuild when libc.a is missing
#    or a patch/shim source is newer than the built artifact.
LIBC_A="$MUSL_DIR/sysroot/lib/libc.a"
NEED_BUILD=0
if [ ! -f "$LIBC_A" ]; then
    NEED_BUILD=1
elif [ "$SCRIPT_DIR/patches/crt_arch.h"     -nt "$LIBC_A" ] \
  || [ "$SCRIPT_DIR/patches/syscall_arch.h" -nt "$LIBC_A" ] \
  || [ "$SCRIPT_DIR/src/shim.c"             -nt "$LIBC_A" ]; then
    NEED_BUILD=1
fi

cd "$MUSL_DIR"
if [ "$NEED_BUILD" -eq 1 ]; then
    echo "==> Building patched musl..."
    make distclean 2>/dev/null || true
    CC="${CROSS}-gcc" \
    ./configure \
        --target=aarch64-linux-musl \
        --disable-shared \
        --prefix="$MUSL_DIR/sysroot"
    # Cross-platform CPU count: nproc (Linux) -> sysctl (macOS) -> 4.
    JOBS=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
    make -j"$JOBS" install
else
    echo "==> musl already built, skipping (delete $LIBC_A to force)"
fi

# 5. Compile hello.c against patched musl.
#
# Object order matters with -nostdlib (we suppress gcc's default startup
# objects so we can use musl's, but then we have to reproduce their order
# manually):
#   crt1.o   — provides _start, must come first
#   crti.o   — opens .init/.fini section prologues
#   <user>   — user objects, between crti.o and crtn.o
#   -lc      — libc, after user objects so user can reference libc symbols
#   -lgcc    — gcc's compiler-rt for low-level helpers
#   crtn.o   — closes .init/.fini section prologues, must come LAST
#
# Putting crtn.o before -lc would seal the .init/.fini sections before any
# libc constructors could be linked in.
echo "==> Compiling hello.c..."
"${CROSS}-gcc" \
    -static \
    -nostdinc -nostdlib \
    -isystem "$MUSL_DIR/sysroot/include" \
    -o "$REPO_ROOT/user/posix-hello/hello" \
    "$MUSL_DIR/sysroot/lib/crt1.o" \
    "$MUSL_DIR/sysroot/lib/crti.o" \
    "$REPO_ROOT/user/posix-hello/hello.c" \
    -L"$MUSL_DIR/sysroot/lib" -lc \
    -lgcc \
    "$MUSL_DIR/sysroot/lib/crtn.o"

echo "==> Built: user/posix-hello/hello"
file "$REPO_ROOT/user/posix-hello/hello"
echo "Done."
