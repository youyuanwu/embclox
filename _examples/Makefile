arch ?= riscv64
board ?= qemu
target := $(arch)gc-unknown-none-elf
# target := $(arch)imac-unknown-none-elf
# when using new toolchain after 2024-04, release mode get stuck
# mode := release
mode := debug
kernel := target/$(target)/$(mode)/e1000-driver-test
bin := target/$(target)/$(mode)/e1000-driver-test.bin
img := target/$(target)/$(mode)/ext4.img

sysroot := $(shell rustc --print sysroot)
objdump := $(shell find $(sysroot) -name llvm-objdump) --arch-name=$(arch)
objcopy := $(shell find $(sysroot) -name llvm-objcopy)

BUILD_ARGS += --target $(target)
ifeq ($(mode), release)
	BUILD_ARGS += --release
endif
ifeq ($(board), fu740)
	BUILD_ARGS += --features board-fu740
endif

.PHONY: kernel build clean qemu run

build: kernel $(bin)

kernel:
	cargo build $(BUILD_ARGS)

$(bin): kernel
	$(objcopy) $(kernel) --strip-all -O binary $(bin)

asm:
	$(objdump) -d $(kernel) | less

sym:
	$(objdump) -t $(kernel) | less

header:
	$(objdump) -x $(kernel) | less

clean:
	cargo clean

qemu:
	qemu-system-$(arch) \
		-machine virt \
		-no-reboot \
		-serial mon:stdio \
		-display none \
		-bios default \
		-kernel $(kernel) \
		-netdev user,id=net0 \
		-device e1000,netdev=net0

run: build qemu
