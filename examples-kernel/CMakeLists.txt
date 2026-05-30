# Unified kernel example: Limine boot, ISO image, QEMU tests.
# Limine binaries are fetched in the root CMakeLists.txt.

set(KERNEL_KERNEL_REL "target/${TARGET}/release/embclox-kernel-example")

add_custom_target(kernel-build
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Building kernel example (Limine BIOS+UEFI)"
)

# --- ISO image (for QEMU and local Hyper-V Gen1) ---

set(KERNEL_ISO "${CMAKE_BINARY_DIR}/kernel.iso")

add_custom_command(
    OUTPUT ${KERNEL_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${KERNEL_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/kernel-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
        ${CMAKE_BINARY_DIR}/kernel-iso/boot/limine/limine.conf
    # UEFI boot files
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/kernel-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/kernel-iso/boot/limine/limine-uefi-cd.bin
    # BIOS boot files (needed for Hyper-V Gen1)
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/kernel-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/kernel-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/kernel-iso -o ${KERNEL_ISO}
        2>/dev/null
    DEPENDS kernel-build ${LIMINE_BOOTX64} ${CMAKE_CURRENT_SOURCE_DIR}/limine.conf
    COMMENT "Creating kernel BIOS+UEFI ISO image"
)
add_custom_target(kernel-image DEPENDS ${KERNEL_ISO})

# --- ISO image (local Hyper-V Gen1 with embclox-test vSwitch IPs) ---

set(KERNEL_HYPERV_ISO "${CMAKE_BINARY_DIR}/kernel-hyperv.iso")

add_custom_command(
    OUTPUT ${KERNEL_HYPERV_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${KERNEL_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_CURRENT_SOURCE_DIR}/limine-hyperv.conf
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/limine/limine.conf
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/limine/limine-uefi-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/kernel-hyperv-iso -o ${KERNEL_HYPERV_ISO}
        2>/dev/null
    DEPENDS kernel-build ${LIMINE_BOOTX64} ${CMAKE_CURRENT_SOURCE_DIR}/limine-hyperv.conf
    COMMENT "Creating kernel BIOS+UEFI ISO image (Hyper-V vSwitch IPs)"
)
add_custom_target(kernel-hyperv-image DEPENDS ${KERNEL_HYPERV_ISO})

# --- ISO image (local Hyper-V Gen1, forces tulip via nic= cmdline) ---
#
# Same kernel binary as kernel-hyperv-image; only the baked-in
# Limine cmdline differs. Use this when the VM has a Hyper-V Legacy
# Network Adapter attached (DEC 21140 emulation) and you want to
# exercise the tulip driver path instead of NetVSC.

set(KERNEL_HYPERV_TULIP_ISO "${CMAKE_BINARY_DIR}/kernel-hyperv-tulip.iso")

add_custom_command(
    OUTPUT ${KERNEL_HYPERV_TULIP_ISO}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/limine
    COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/EFI/BOOT
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_SOURCE_DIR}/${KERNEL_KERNEL_REL}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/kernel
    COMMAND ${CMAKE_COMMAND} -E copy
        ${CMAKE_CURRENT_SOURCE_DIR}/limine-hyperv-tulip.conf
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/limine/limine.conf
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BOOTX64}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/EFI/BOOT/BOOTX64.EFI
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_UEFI_CD}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/limine/limine-uefi-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_CD}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/limine/limine-bios-cd.bin
    COMMAND ${CMAKE_COMMAND} -E copy
        ${LIMINE_BIOS_SYS}
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso/boot/limine/limine-bios.sys
    COMMAND xorriso -as mkisofs
        -b boot/limine/limine-bios-cd.bin
        -no-emul-boot -boot-load-size 4 -boot-info-table
        --efi-boot boot/limine/limine-uefi-cd.bin
        -efi-boot-part --efi-boot-image --protective-msdos-label
        ${CMAKE_BINARY_DIR}/kernel-hyperv-tulip-iso -o ${KERNEL_HYPERV_TULIP_ISO}
        2>/dev/null
    DEPENDS kernel-build ${LIMINE_BOOTX64} ${CMAKE_CURRENT_SOURCE_DIR}/limine-hyperv-tulip.conf
    COMMENT "Creating kernel BIOS+UEFI ISO image (Hyper-V vSwitch IPs, nic=tulip)"
)
add_custom_target(kernel-hyperv-tulip-image DEPENDS ${KERNEL_HYPERV_TULIP_ISO})

# --- VHD image (for Azure / local Hyper-V Gen1 deployment) ---
#
# Uses limine-azure.conf which boots the kernel with `cmdline: net=dhcp`.
# Azure has a production-grade DHCP server, so DHCP is the right choice.
# For local Hyper-V dev use the ISO instead — it defaults to static IP
# on the dedicated `embclox-test` Internal vSwitch.

set(KERNEL_VHD "${CMAKE_BINARY_DIR}/kernel.vhd")

add_custom_command(
    OUTPUT ${KERNEL_VHD}
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/mkvhd.sh
        ${CMAKE_SOURCE_DIR}/${KERNEL_KERNEL_REL}
        ${limine_SOURCE_DIR}
        ${CMAKE_CURRENT_SOURCE_DIR}/limine-azure.conf
        ${KERNEL_VHD}
    DEPENDS kernel-build ${CMAKE_CURRENT_SOURCE_DIR}/limine-azure.conf
    COMMENT "Creating kernel VHD image (Azure Gen1, DHCP cmdline)"
)
add_custom_target(kernel-vhd DEPENDS ${KERNEL_VHD})

# --- QEMU run targets ---

# Default: e1000 (matches limine.conf's static 10.0.2.15/24).
add_custom_target(qemu-kernel
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive if=pflash,format=raw,readonly=on,file=${OVMF_CODE}
        -cdrom ${KERNEL_ISO}
        -netdev user,id=net0,hostfwd=tcp::5555-:1234
        -device e1000,netdev=net0
        -m 256M
    DEPENDS kernel-image
    COMMENT "Running kernel example in QEMU (UEFI + e1000 NIC)"
)

# Tulip (DEC 21140) variant.
add_custom_target(qemu-kernel-tulip
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive if=pflash,format=raw,readonly=on,file=${OVMF_CODE}
        -cdrom ${KERNEL_ISO}
        -netdev user,id=net0,hostfwd=tcp::5555-:1234
        -device tulip,netdev=net0
        -m 256M
    DEPENDS kernel-image
    COMMENT "Running kernel example in QEMU (UEFI + tulip NIC)"
)

# --- CTest tests ---

add_test(NAME kernel-echo-e1000
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${KERNEL_ISO}
        --iso
        --probe tcp:5555:hello-embclox
        --timeout 30
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5555-:1234 -device e1000,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_test(NAME kernel-echo-tulip
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${KERNEL_ISO}
        --iso
        --probe tcp:5555:hello-embclox
        --timeout 30
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5555-:1234 -device tulip,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
