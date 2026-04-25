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
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Formatting and linting all crates"
)

add_custom_target(example-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/examples
    COMMENT "Building embclox example (${MODE})"
)

add_custom_target(image
    COMMAND cargo run -p embclox-mkimage -- ${KERNEL_REL} ${IMAGE_REL}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS example-build
    COMMENT "Creating BIOS disk image"
)

add_custom_target(qemu
    COMMAND qemu-system-x86_64
        -machine q35
        -no-reboot
        -serial mon:stdio
        -display none
        -drive format=raw,file=${CMAKE_SOURCE_DIR}/${IMAGE_REL}
        -netdev user,id=net0,hostfwd=tcp::5555-:1234
        -device e1000,netdev=net0
    DEPENDS image
    COMMENT "Running QEMU"
)

set(UNIT_KERNEL_REL "target/${TARGET}/${MODE}/embclox-unit-tests")
set(UNIT_IMAGE_REL "${UNIT_KERNEL_REL}.img")

add_custom_target(unit-test-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/qemu-tests/unit
    COMMENT "Building unit tests (${MODE})"
)

add_custom_target(unit-test-image
    COMMAND cargo run -p embclox-mkimage -- ${UNIT_KERNEL_REL} ${UNIT_IMAGE_REL}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS unit-test-build
    COMMENT "Creating unit test disk image"
)

# CTest tests — run with: ctest --test-dir build
add_test(NAME unit
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${CMAKE_SOURCE_DIR}/${UNIT_IMAGE_REL}
        --qemu-args "-netdev user,id=net0 -device e1000,netdev=net0"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_test(NAME integration
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${CMAKE_SOURCE_DIR}/${IMAGE_REL}
        --probe tcp:5555:hello-embclox
        --qemu-args "-netdev user,id=net0,hostfwd=tcp::5555-:1234 -device e1000,netdev=net0"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
