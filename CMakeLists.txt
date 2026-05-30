cmake_minimum_required(VERSION 3.16)
project(embclox NONE)

enable_testing()

set(ARCH "x86_64" CACHE STRING "Target architecture")
set(TARGET "${ARCH}-unknown-none")
set(MODE "debug" CACHE STRING "Build mode: debug or release")
set(OVMF_CODE "/usr/share/OVMF/OVMF_CODE_4M.fd")

# --- Shared Limine bootloader binaries ---
include(FetchContent)
FetchContent_Declare(limine
    URL https://github.com/Limine-Bootloader/Limine/releases/download/v12.0.2/limine-binary.tar.gz
    DOWNLOAD_EXTRACT_TIMESTAMP ON
)
FetchContent_MakeAvailable(limine)

set(LIMINE_BOOTX64 "${limine_SOURCE_DIR}/BOOTX64.EFI")
set(LIMINE_UEFI_CD "${limine_SOURCE_DIR}/limine-uefi-cd.bin")
set(LIMINE_BIOS_CD "${limine_SOURCE_DIR}/limine-bios-cd.bin")
set(LIMINE_BIOS_SYS "${limine_SOURCE_DIR}/limine-bios.sys")

# Lint all crates
add_custom_target(check
    COMMAND cargo fmt --all
    COMMAND cargo clippy -- -D warnings
    COMMAND cargo clippy --manifest-path examples-e1000/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path qemu-tests/unit/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path examples-kernel/Cargo.toml
        --target ${TARGET} -- -D warnings
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Formatting and linting all crates"
)

# e1000 example
add_subdirectory(examples-e1000)

# Unified kernel example (Limine boot, ISO; PCI + VMBus via the driver registry)
add_subdirectory(examples-kernel)

# Shared unit-test harness (Limine boot, ISO; e1000 NIC + isa-debug-exit)
add_subdirectory(qemu-tests/unit)

# Aggregate: build every bootable image (ISOs + VHDs)
add_custom_target(images
    DEPENDS
        e1000-image
        unit-test-image
        kernel-image
        kernel-hyperv-image
        kernel-vhd
    COMMENT "Building all bootable images (ISOs + VHDs)"
)
