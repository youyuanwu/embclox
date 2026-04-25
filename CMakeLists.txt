cmake_minimum_required(VERSION 3.16)
project(uos NONE)

set(ARCH "x86_64" CACHE STRING "Target architecture")
set(TARGET "${ARCH}-unknown-none")
set(MODE "debug" CACHE STRING "Build mode: debug or release")
set(KERNEL_REL "target/${TARGET}/${MODE}/e1000-embassy-example")
set(IMAGE_REL "${KERNEL_REL}.img")

set(BUILD_ARGS "")
if(MODE STREQUAL "release")
    list(APPEND BUILD_ARGS "--release")
endif()

add_custom_target(check
    COMMAND cargo fmt --all
    COMMAND cargo clippy -- -D warnings
    COMMAND cargo clippy --manifest-path _examples_embassy/Cargo.toml
        --target ${TARGET} -- -D warnings
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Formatting and linting all crates"
)

add_custom_target(example-build
    COMMAND cargo build ${BUILD_ARGS}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/_examples_embassy
    COMMENT "Building e1000-embassy example (${MODE})"
)

add_custom_target(image
    COMMAND cargo run -p mkimage -- ${KERNEL_REL} ${IMAGE_REL}
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

add_custom_target(test
    COMMAND bash test.sh
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/_examples_embassy
    DEPENDS image
    COMMENT "Running QEMU integration test"
)
