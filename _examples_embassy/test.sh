#!/bin/bash
# test.sh — Run the e1000-embassy example on QEMU and verify TCP echo.
#
# Usage: ./test.sh [--release]
# Requires: qemu-system-x86_64, nc (netcat)
# Expects the disk image to be pre-built (via cmake --build build --target image)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

MODE=debug
if [[ "${1:-}" == "--release" ]]; then
    MODE=release
fi

HOST_PORT=5555
GUEST_PORT=1234
TIMEOUT=30
TEST_STRING="hello-embassy-e1000"

IMAGE="$SCRIPT_DIR/../target/x86_64-unknown-none/${MODE}/e1000-embassy-example.img"

if [[ ! -f "$IMAGE" ]]; then
    echo "ERROR: disk image not found at $IMAGE"
    exit 1
fi

echo "=== Starting QEMU ==="
qemu-system-x86_64 \
    -machine q35 \
    -no-reboot \
    -serial mon:stdio \
    -display none \
    -drive format=raw,file="$IMAGE" \
    -netdev user,id=net0,hostfwd=tcp::${HOST_PORT}-:${GUEST_PORT} \
    -device e1000,netdev=net0 \
    > /tmp/qemu-e1000-test.log 2>&1 &
QEMU_PID=$!

cleanup() {
    kill "$QEMU_PID" 2>/dev/null || true
    wait "$QEMU_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Waiting for guest to boot (8s) ==="
sleep 8

# Check QEMU is still running
if ! kill -0 "$QEMU_PID" 2>/dev/null; then
    echo "ERROR: QEMU exited early. Log:"
    cat /tmp/qemu-e1000-test.log
    exit 1
fi

echo "=== Testing TCP echo on localhost:${HOST_PORT} ==="
RESPONSE=$(echo "$TEST_STRING" | timeout 10 nc -w 3 localhost "$HOST_PORT" 2>/dev/null || true)

if [[ "$RESPONSE" == "$TEST_STRING" ]]; then
    echo "=== PASS: TCP echo returned '$RESPONSE' ==="
    exit 0
else
    echo "=== FAIL: expected '$TEST_STRING', got '$RESPONSE' ==="
    echo "QEMU log:"
    cat /tmp/qemu-e1000-test.log
    exit 1
fi
