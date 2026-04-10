KERNEL_ELF := target/aarch64-unknown-none/debug/lockjaw
KERNEL_ELF_RELEASE := target/aarch64-unknown-none/release/lockjaw

QEMU := qemu-system-aarch64
QEMU_FLAGS := -machine virt,gic-version=3 -cpu cortex-a53 -nographic -kernel

.PHONY: build build-release run run-release objdump nm check-stack clean

build: check-stack
	cargo build

build-release: check-stack
	cargo build --release

run: build
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF)

run-release: build-release
	$(QEMU) $(QEMU_FLAGS) $(KERNEL_ELF_RELEASE)

objdump: build
	cargo objdump -- -d | head -80

nm: build
	cargo nm -- --defined-only

check-stack:
	cargo xtask check-stack

clean:
	cargo clean
