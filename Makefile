KERNEL_ELF := target/aarch64-unknown-none-softfloat/debug/lockjaw
KERNEL_ELF_RELEASE := target/aarch64-unknown-none-softfloat/release/lockjaw

QEMU := qemu-system-aarch64
# Two UARTs: UART0 (kernel debug) and UART1 (userspace driver), both to stdio.
# Ctrl-A C switches between QEMU monitor and serial mux.
QEMU_FLAGS := -machine virt,gic-version=3 -cpu cortex-a53 -display none \
	-chardev stdio,mux=on,id=char0 -mon chardev=char0,mode=readline \
	-serial chardev:char0 -serial chardev:char0 \
	-kernel

INIT_ELF := user/init/target/aarch64-unknown-none/release/lockjaw-init

# ramfb display: add -device ramfb and a display backend.
QEMU_DISPLAY_FLAGS := -machine virt,gic-version=3 -cpu cortex-a53 -m 128M \
	-chardev stdio,mux=on,id=char0 -mon chardev=char0,mode=readline \
	-serial chardev:char0 -serial chardev:char0 \
	-device ramfb -display cocoa \
	-kernel

USER_CRATES := user/hello user/uart-driver user/device-manager user/ramfb-driver user/display-test user/virtio-blk-driver user/fat32-server user/fat32-test user/posix-server user/cprman-driver user/clock-test user/emmc2-driver user/sleep-test user/partition-manager user/init

.PHONY: build build-release build-user build-hash clean-all run run-release run-display run-blk objdump nm check-stack check-pointers check-vtables check-init-size check-linker-symbols check-kernel-no-neon check-driver-unsafe test test-unit test-qemu-gicv3 test-qemu-gicv2 clean pi4 test-img

clean-all:
	cargo clean
	@for d in $(USER_CRATES); do (cd $$d && cargo clean 2>/dev/null); done

build-hash:
	@mkdir -p target
	@find src/ lockjaw-types/src/ user/*/src/ -name '*.rs' 2>/dev/null | sort | xargs cat | shasum -a 256 | cut -c1-16 > target/source-hash.txt.tmp
	@cmp -s target/source-hash.txt.tmp target/source-hash.txt 2>/dev/null || mv target/source-hash.txt.tmp target/source-hash.txt
	@rm -f target/source-hash.txt.tmp

build-user: clean-all build-hash
	cd user/hello && cargo build --release
	cd user/uart-driver && cargo build --release
	cd user/device-manager && cargo build --release
	cd user/ramfb-driver && cargo build --release
	cd user/display-test && cargo build --release
	cd user/virtio-blk-driver && cargo build --release
	cd user/fat32-server && cargo build --release
	cd user/fat32-test && cargo build --release
	./musl-lockjaw/build.sh
	cd user/posix-server && cargo build --release
	cd user/cprman-driver && cargo build --release
	cd user/clock-test && cargo build --release
	cd user/emmc2-driver && cargo build --release
	cd user/sleep-test && cargo build --release
	cd user/neon-canary && cargo build --release
	cd user/partition-manager && cargo build --release
	cd user/init && cargo build --release

build: build-user check-stack check-pointers check-init-size check-linker-symbols
	cargo xtask gen-regs --check
	cargo xtask gen-wires --check
	cargo xtask check-driver-unsafe
	cargo build
	cargo xtask check-vtables
	cargo xtask check-kernel-no-neon

build-release: build-user check-stack
	cargo xtask gen-regs --check
	cargo xtask gen-wires --check
	cargo build --release
	cargo xtask check-kernel-no-neon

run: build
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF)

run-release: build-release
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF_RELEASE)

run-display: build
	$(QEMU) $(QEMU_DISPLAY_FLAGS) $(KERNEL_ELF)

run-blk: build test-img
	$(QEMU) -machine virt,gic-version=3 -cpu cortex-a53 -display none \
		-chardev stdio,mux=on,id=char0 -mon chardev=char0,mode=readline \
		-serial chardev:char0 -serial chardev:char0 \
		-global virtio-mmio.force-legacy=false \
		-drive file=test.img,format=raw,if=none,id=blk0 \
		-device virtio-blk-device,drive=blk0 \
		-kernel $(KERNEL_ELF)

# 64 MiB FAT32 image. 64 MiB sits comfortably above FAT32's
# cluster-count minimum (~33 MiB) so mformat produces a clean FAT32
# volume with no FAT12/16 fallback. Phase A only needs the image to
# exist for the block driver's selftest read; later phases populate
# it with files via mcopy.
#
# Rebuilds only if missing or if the file size doesn't match (so a
# stale 1 MiB image from earlier runs gets replaced automatically).
# Requires mtools installed on the host (mformat + mcopy):
#   macOS: brew install mtools
#   Debian/Ubuntu: apt install mtools
test-img:
	@need=0; \
	if [ ! -f test.img ]; then \
		need=1; \
	elif [ "$$(wc -c < test.img | tr -d ' ')" != "67108864" ]; then \
		need=1; \
	elif [ "$$(head -c 3 test.img | xxd -p)" != "eb5890" ]; then \
		need=1; \
	elif ! mdir -i test.img ::HELLO.TXT >/dev/null 2>&1; then \
		need=1; \
	fi; \
	if [ $$need -eq 1 ]; then \
		echo "Creating 64 MiB FAT32 test.img with HELLO.TXT..."; \
		dd if=/dev/zero of=test.img bs=1M count=64 status=none; \
		mformat -F -i test.img -v LOCKJAW ::; \
		printf 'hello from fat32\n' | mcopy -i test.img -o - ::HELLO.TXT; \
	fi

objdump: build
	cargo objdump -- -d | head -80

nm: build
	cargo nm -- --defined-only

check-stack:
	cargo xtask check-stack

check-pointers:
	cargo xtask check-pointers

check-vtables:
	cargo xtask check-vtables

check-init-size:
	cargo xtask check-init-size

check-linker-symbols:
	cargo xtask check-linker-symbols

check-kernel-no-neon:
	cargo xtask check-kernel-no-neon

check-driver-unsafe:
	cargo xtask check-driver-unsafe

test: test-unit test-qemu-gicv3 test-qemu-gicv2

test-unit:
	cargo test -p lockjaw-types --target aarch64-apple-darwin
	cargo test --manifest-path user/lockjaw-mmio/Cargo.toml --target aarch64-apple-darwin
	cargo test --manifest-path user/lockjaw-regs/Cargo.toml --target aarch64-apple-darwin

test-qemu-gicv3: build test-img
	GIC_VERSION=3 bash tests/qemu_integration.sh

test-qemu-gicv2: build test-img
	GIC_VERSION=2 bash tests/qemu_integration.sh

kernel8.img: build-release
	rust-objcopy -O binary $(KERNEL_ELF_RELEASE) kernel8.img

pi4: kernel8.img
	@echo "kernel8.img ready — copy to Pi 4B SD card boot partition"

clean:
	cargo clean
