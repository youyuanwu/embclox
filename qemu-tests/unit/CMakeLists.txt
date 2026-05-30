# Unit-test kernel ISO + QEMU lane.
#
# Builds qemu-tests/unit (a Limine-booted no_std test harness) and
# wraps it in a BIOS+UEFI ISO that runs under QEMU against an e1000
# NIC. The kernel writes pass/fail via isa-debug-exit so the QEMU
# exit code is the test verdict.
#
# Limine binaries are fetched in the root CMakeLists.txt.

set(UNIT_KERNEL_REL "target/${TARGET}/release/embclox-unit-tests")

add_custom_target(unit-test-build
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
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
        ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
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
    DEPENDS unit-test-build ${LIMINE_BOOTX64} ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
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
