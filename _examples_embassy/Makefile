arch ?= x86_64
target := $(arch)-unknown-none
mode := debug
target_dir := ../target
kernel := $(target_dir)/$(target)/$(mode)/e1000-embassy-example
image := $(kernel).img

BUILD_ARGS =
ifeq ($(mode), release)
	BUILD_ARGS += --release
endif

.PHONY: build image clean qemu run

build:
	cargo build $(BUILD_ARGS)

image: build
	cd .. && cargo run -p mkimage -- \
		_examples_embassy/$(kernel) _examples_embassy/$(image)

clean:
	cd .. && cargo clean

qemu: image
	qemu-system-x86_64 \
		-machine q35 \
		-no-reboot \
		-serial mon:stdio \
		-display none \
		-drive format=raw,file=$(image) \
		-netdev user,id=net0,hostfwd=tcp::5555-:1234 \
		-device e1000,netdev=net0

run: qemu
