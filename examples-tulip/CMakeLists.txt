# Tulip example: Limine boot, ISO/VHD images, QEMU tests

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
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Building Tulip example (Limine BIOS+UEFI)"
)

# --- ISO image (for QEMU and local Hyper-V) ---

set(TULIP_ISO "${CMAKE_BINARY_DIR}/tulip.iso")

add_custom_command(
    OUTPUT ${TULIP_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/tulip-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/tulip-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${TULIP_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/tulip-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
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

# --- VHD image (for Hyper-V / Azure Gen1) ---

set(TULIP_VHD "${CMAKE_BINARY_DIR}/tulip.vhd")

add_custom_command(
    OUTPUT ${TULIP_VHD}
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/mkvhd.sh
        ${CMAKE_SOURCE_DIR}/${TULIP_KERNEL_REL}
        ${limine_SOURCE_DIR}
        ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
        ${TULIP_VHD}
    DEPENDS tulip-build
    COMMENT "Creating Tulip VHD image (BIOS+UEFI)"
)
add_custom_target(tulip-vhd DEPENDS ${TULIP_VHD})

# --- QEMU run target ---

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

# --- CTest tests ---

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
