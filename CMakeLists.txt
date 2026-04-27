cmake_minimum_required(VERSION 3.16)
project(embclox NONE)

enable_testing()

set(ARCH "x86_64" CACHE STRING "Target architecture")
set(TARGET "${ARCH}-unknown-none")
set(MODE "debug" CACHE STRING "Build mode: debug or release")
set(OVMF_CODE "/usr/share/OVMF/OVMF_CODE_4M.fd")

# Lint all crates
add_custom_target(check
    COMMAND cargo fmt --all
    COMMAND cargo clippy -- -D warnings
    COMMAND cargo clippy --manifest-path examples-e1000/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path qemu-tests/unit/Cargo.toml
        --target ${TARGET} -- -D warnings
    COMMAND cargo clippy --manifest-path examples-tulip/Cargo.toml
        --target ${TARGET} -- -D warnings
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Formatting and linting all crates"
)

# e1000 example + unit tests
add_subdirectory(examples-e1000)

# Tulip example (Limine boot, ISO/VHD)
add_subdirectory(examples-tulip)
