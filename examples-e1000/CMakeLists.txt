# e1000 example: Limine boot, ISO image, QEMU tests
# Limine binaries are fetched in the root CMakeLists.txt

set(E1000_KERNEL_REL "target/${TARGET}/release/embclox-example")

add_custom_target(e1000-build
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Building e1000 example (Limine BIOS+UEFI)"
)

# --- ISO image (for QEMU and local Hyper-V Gen1) ---

set(E1000_ISO "${CMAKE_BINARY_DIR}/e1000.iso")

add_custom_command(
    OUTPUT ${E1000_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/e1000-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/e1000-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${E1000_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/e1000-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
        ${CMAKE_BINARY_DIR}/e1000-iso/boot/limine/limine.conf
    # UEFI boot files
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/e1000-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/e1000-iso/boot/limine/limine-uefi-cd.bin
    # BIOS boot files (needed for Hyper-V Gen1)
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/e1000-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/e1000-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/e1000-iso -o ${E1000_ISO}
        2>/dev/null
    DEPENDS e1000-build ${LIMINE_BOOTX64} ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
    COMMENT "Creating e1000 BIOS+UEFI ISO image"
)
add_custom_target(e1000-image DEPENDS ${E1000_ISO})

# --- QEMU run target ---

add_custom_target(qemu-e1000
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive if=pflash,format=raw,readonly=on,file=${OVMF_CODE}
        -cdrom ${E1000_ISO}
        -netdev user,id=net0,hostfwd=tcp::5555-:1234
        -device e1000,netdev=net0
        -m 256M
    DEPENDS e1000-image
    COMMENT "Running e1000 example in QEMU (UEFI + e1000 NIC)"
)

# --- CTest tests ---

add_test(NAME e1000-echo
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${E1000_ISO}
        --iso
        --probe tcp:5555:hello-embclox
        --timeout 30
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5555-:1234 -device e1000,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

# --- Unit tests (built from qemu-tests/unit, run against e1000 NIC) ---

set(UNIT_KERNEL_REL "target/${TARGET}/release/embclox-unit-tests")

add_custom_target(unit-test-build
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/qemu-tests/unit
    COMMENT "Building unit tests (Limine BIOS+UEFI)"
)

set(UNIT_ISO "${CMAKE_BINARY_DIR}/unit-tests.iso")

add_custom_command(
    OUTPUT ${UNIT_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/unit-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/unit-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${UNIT_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/unit-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/qemu-tests/unit/limine.conf
        ${CMAKE_BINARY_DIR}/unit-iso/boot/limine/limine.conf
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/unit-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/unit-iso/boot/limine/limine-uefi-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/unit-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/unit-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/unit-iso -o ${UNIT_ISO}
        2>/dev/null
    DEPENDS unit-test-build ${LIMINE_BOOTX64} ${CMAKE_SOURCE_DIR}/qemu-tests/unit/limine.conf
    COMMENT "Creating unit-tests BIOS+UEFI ISO image"
)
add_custom_target(unit-test-image DEPENDS ${UNIT_ISO})

add_test(NAME unit
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${UNIT_ISO}
        --iso
        --qemu-args "-netdev user,id=net0 -device e1000,netdev=net0 -m 256M -device isa-debug-exit,iobase=0xf4,iosize=0x04"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
