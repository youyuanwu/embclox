cmake_minimum_required(VERSION 3.16)
project(embclox NONE)

enable_testing()

set(ARCH "x86_64" CACHE STRING "Target architecture")
set(TARGET "${ARCH}-unknown-none")
set(MODE "debug" CACHE STRING "Build mode: debug or release")
set(KERNEL_REL "target/${TARGET}/${MODE}/embclox-example")
set(IMAGE_REL "${KERNEL_REL}.img")

set(BUILD_ARGS "")
if(MODE STREQUAL "release")
    list(APPEND BUILD_ARGS "--release")
endif()

add_custom_target(check
    COMMAND cargo fmt --all
    COMMAND cargo clippy -- -D warnings
    COMMAND cargo clippy --manifest-path examples/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path qemu-tests/unit/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path examples-tulip/Cargo.toml
        --target ${TARGET} -- -D warnings
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Formatting and linting all crates"
)

set(IMAGE_ABS "${CMAKE_SOURCE_DIR}/${IMAGE_REL}")

add_custom_target(example-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/examples
    COMMENT "Building embclox example (${MODE})"
)

add_custom_command(
    OUTPUT ${IMAGE_ABS}
    COMMAND cargo run -p embclox-mkimage -- ${KERNEL_REL} ${IMAGE_REL}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS example-build
    COMMENT "Creating UEFI disk image"
)
add_custom_target(example-image DEPENDS ${IMAGE_ABS})

set(OVMF_CODE "/usr/share/OVMF/OVMF_CODE_4M.fd")

add_custom_target(qemu
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive if=pflash,format=raw,readonly=on,file=${OVMF_CODE}
        -drive format=raw,file=${IMAGE_ABS}
        -netdev user,id=net0,hostfwd=tcp::5555-:1234
        -device e1000,netdev=net0
    DEPENDS example-image
    COMMENT "Running QEMU (UEFI)"
)

set(UNIT_KERNEL_REL "target/${TARGET}/${MODE}/embclox-unit-tests")
set(UNIT_IMAGE_REL "${UNIT_KERNEL_REL}.img")
set(UNIT_IMAGE_ABS "${CMAKE_SOURCE_DIR}/${UNIT_IMAGE_REL}")

add_custom_target(unit-test-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/qemu-tests/unit
    COMMENT "Building unit tests (${MODE})"
)

add_custom_command(
    OUTPUT ${UNIT_IMAGE_ABS}
    COMMAND cargo run -p embclox-mkimage -- ${UNIT_KERNEL_REL} ${UNIT_IMAGE_REL}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS unit-test-build
    COMMENT "Creating unit test UEFI disk image"
)
add_custom_target(unit-test-image DEPENDS ${UNIT_IMAGE_ABS})

# CTest tests — run with: ctest --test-dir build
add_test(NAME unit
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${UNIT_IMAGE_ABS}
        --qemu-args "-netdev user,id=net0 -device e1000,netdev=net0"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_test(NAME integration
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${IMAGE_ABS}
        --probe tcp:5555:hello-embclox
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5555-:1234 -device e1000,netdev=net0"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

# --- Tulip example (Limine UEFI boot) ---

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

set(TULIP_KERNEL_REL "target/${TARGET}/release/embclox-tulip-example")

add_custom_target(tulip-build
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/examples-tulip
    COMMENT "Building Tulip example (Limine BIOS+UEFI)"
)

# Build the Limine ISO image (BIOS + UEFI dual boot)
set(TULIP_ISO "${CMAKE_BINARY_DIR}/tulip.iso")

add_custom_command(
    OUTPUT ${TULIP_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/tulip-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${TULIP_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/examples-tulip/limine.conf
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine/limine.conf
    # UEFI boot files
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/tulip-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine/limine-uefi-cd.bin
    # BIOS boot files (needed for Hyper-V Gen1)
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/tulip-iso -o ${TULIP_ISO}
        2>/dev/null
    DEPENDS tulip-build ${LIMINE_BOOTX64}
    COMMENT "Creating Tulip BIOS+UEFI ISO image"
)
add_custom_target(tulip-image DEPENDS ${TULIP_ISO})

add_custom_target(qemu-tulip
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive if=pflash,format=raw,readonly=on,file=${OVMF_CODE}
        -cdrom ${TULIP_ISO}
        -netdev user,id=net0
        -device tulip,netdev=net0
        -m 256M
    DEPENDS tulip-image
    COMMENT "Running Tulip example in QEMU (UEFI + Tulip NIC)"
)

add_test(NAME tulip-boot
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${TULIP_ISO}
        --iso
        --log-match "TULIP INIT PASSED"
        --timeout 15
        --qemu-args "-netdev user,id=net0 -device tulip,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_test(NAME tulip-echo
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${TULIP_ISO}
        --iso
        --probe tcp:5556:hello-tulip
        --timeout 30
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5556-:1234 -device tulip,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
