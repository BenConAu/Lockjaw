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

.PHONY: build build-release build-user run run-release objdump nm check-stack test test-unit test-qemu clean

build-user:
	cd user/hello && cargo build --release
	cd user/uart-driver && cargo build --release
	cd user/init && cargo build --release

build: build-user check-stack
	cargo build

build-release: build-user check-stack
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

test: test-unit test-qemu

test-unit:
	cargo test -p lockjaw-types --target x86_64-apple-darwin

test-qemu: build
	bash tests/qemu_integration.sh

clean:
	cargo clean
