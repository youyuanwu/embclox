# e1000 example: UEFI disk image + QEMU tests

set(KERNEL_REL "target/${TARGET}/${MODE}/embclox-example")
set(IMAGE_REL "${KERNEL_REL}.img")
set(IMAGE_ABS "${CMAKE_SOURCE_DIR}/${IMAGE_REL}")

set(BUILD_ARGS "")
if(MODE STREQUAL "release")
    list(APPEND BUILD_ARGS "--release")
endif()

add_custom_target(example-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Building e1000 example (${MODE})"
)

add_custom_command(
    OUTPUT ${IMAGE_ABS}
    COMMAND cargo run -p embclox-mkimage -- ${KERNEL_REL} ${IMAGE_REL}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS example-build
    COMMENT "Creating e1000 UEFI disk image"
)
add_custom_target(example-image DEPENDS ${IMAGE_ABS})

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
    COMMENT "Running e1000 example in QEMU (UEFI)"
)

# Unit tests
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

# CTest tests
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
