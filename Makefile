KERNEL_ELF := target/aarch64-unknown-none/debug/lockjaw
KERNEL_ELF_RELEASE := target/aarch64-unknown-none/release/lockjaw

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

USER_CRATES := user/hello user/uart-driver user/device-manager user/ramfb-driver user/display-test user/virtio-blk-driver user/init

.PHONY: build build-release build-user build-hash clean-all run run-release run-display run-blk objdump nm check-stack check-pointers check-vtables test test-unit test-qemu-gicv3 test-qemu-gicv2 clean pi4

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
	cd user/init && cargo build --release

build: build-user check-stack check-pointers
	cargo build
	cargo xtask check-vtables

build-release: build-user check-stack
	cargo build --release

run: build
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF)

run-release: build-release
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF_RELEASE)

run-display: build
	$(QEMU) $(QEMU_DISPLAY_FLAGS) $(KERNEL_ELF)

run-blk: build
	@test -f test.img || dd if=/dev/zero of=test.img bs=1M count=1 2>/dev/null
	$(QEMU) -machine virt,gic-version=3 -cpu cortex-a53 -display none \
		-chardev stdio,mux=on,id=char0 -mon chardev=char0,mode=readline \
		-serial chardev:char0 -serial chardev:char0 \
		-global virtio-mmio.force-legacy=false \
		-drive file=test.img,format=raw,if=none,id=blk0 \
		-device virtio-blk-device,drive=blk0 \
		-kernel $(KERNEL_ELF)

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

test: test-unit test-qemu-gicv3 test-qemu-gicv2

test-unit:
	cargo test -p lockjaw-types --target aarch64-apple-darwin

test-qemu-gicv3: build
	GIC_VERSION=3 bash tests/qemu_integration.sh

test-qemu-gicv2: build
	GIC_VERSION=2 bash tests/qemu_integration.sh

kernel8.img: build-release
	rust-objcopy -O binary $(KERNEL_ELF_RELEASE) kernel8.img

pi4: kernel8.img
	@echo "kernel8.img ready — copy to Pi 4B SD card boot partition"

clean:
	cargo clean
