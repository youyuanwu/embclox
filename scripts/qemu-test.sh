#!/bin/bash
# qemu-test.sh — Shared QEMU test runner for embclox.
#
# Usage:
#   qemu-test.sh <image> [OPTIONS]
#
# Modes:
#   --probe tcp:PORT:STRING   Send STRING via TCP, expect echo back
#   --log-match PATTERN       Scan serial log for regex (anchored)
#   (no mode flags)           Just check QEMU exit code via isa-debug-exit
#
# Options:
#   --timeout SECS            Max wait (default: 30)
#   --qemu-args "ARGS"        Extra QEMU arguments (e.g., "-device e1000,netdev=net0")
#
# Exit code:
#   0 = pass, 1 = fail
#
# The script adds -device isa-debug-exit and remaps QEMU exit codes:
#   Guest writes 0 → QEMU exits 1 → remapped to 0 (success)
#   Guest writes 1 → QEMU exits 3 → remapped to 1 (failure)

set -uo pipefail

die() { echo "ERROR: $*" >&2; exit 1; }

# --- Parse arguments ---
IMAGE=""
PROBE_PORT=""
PROBE_STRING=""
LOG_PATTERN=""
TIMEOUT=30
EXTRA_QEMU_ARGS=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --probe)
            shift
            # Parse tcp:PORT:STRING
            IFS=: read -r proto port string <<< "$1"
            [[ "$proto" == "tcp" ]] || die "probe must be tcp:PORT:STRING"
            PROBE_PORT="$port"
            PROBE_STRING="$string"
            ;;
        --log-match)
            shift
            LOG_PATTERN="$1"
            ;;
        --timeout)
            shift
            TIMEOUT="$1"
            ;;
        --qemu-args)
            shift
            EXTRA_QEMU_ARGS="$1"
            ;;
        -*)
            die "unknown option: $1"
            ;;
        *)
            [[ -z "$IMAGE" ]] || die "multiple images specified"
            IMAGE="$1"
            ;;
    esac
    shift
done

[[ -n "$IMAGE" ]] || die "usage: qemu-test.sh <image> [OPTIONS]"
[[ -f "$IMAGE" ]] || die "image not found: $IMAGE"

# Place log next to the image in the build directory
IMAGE_DIR="$(dirname "$IMAGE")"
IMAGE_BASE="$(basename "$IMAGE" .img)"
LOG="${IMAGE_DIR}/${IMAGE_BASE}-qemu.log"

cleanup() {
    if [[ -n "${QEMU_PID:-}" ]]; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# --- Build QEMU command ---
QEMU_CMD=(
    qemu-system-x86_64
    -machine q35
    -no-reboot
    -serial "file:$LOG"
    -display none
    -drive "format=raw,file=$IMAGE"
    -device "isa-debug-exit,iobase=0xf4,iosize=0x04"
)

# Add extra args (word-split intentionally)
if [[ -n "$EXTRA_QEMU_ARGS" ]]; then
    read -ra extra <<< "$EXTRA_QEMU_ARGS"
    QEMU_CMD+=("${extra[@]}")
fi

# --- Probe mode: QEMU runs in background, we probe from host ---
if [[ -n "$PROBE_PORT" ]]; then
    echo "=== Starting QEMU (probe mode, port $PROBE_PORT) ==="
    "${QEMU_CMD[@]}" > /dev/null 2>&1 &
    QEMU_PID=$!

    echo "=== Waiting for guest to boot (8s) ==="
    sleep 8

    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "ERROR: QEMU exited early. Log:"
        cat "$LOG"
        exit 1
    fi

    echo "=== Probing tcp:localhost:${PROBE_PORT} ==="
    RESPONSE=$(echo "$PROBE_STRING" | timeout 10 nc -w 3 localhost "$PROBE_PORT" 2>/dev/null || true)

    if [[ "$RESPONSE" == "$PROBE_STRING" ]]; then
        echo "=== PASS: TCP echo returned '$RESPONSE' ==="
        exit 0
    else
        echo "=== FAIL: expected '$PROBE_STRING', got '$RESPONSE' ==="
        echo "QEMU log:"
        cat "$LOG"
        exit 1
    fi
fi

# --- Exit-code / log-match mode: QEMU runs to completion ---
echo "=== Starting QEMU ==="
set +e
timeout "$TIMEOUT" "${QEMU_CMD[@]}" > /dev/null 2>&1
QEMU_RC=$?
set -e

# Capture actual exit code (timeout returns 124 on timeout)
if [[ $QEMU_RC -eq 124 ]]; then
    echo "=== FAIL: QEMU timed out after ${TIMEOUT}s ==="
    echo "QEMU log:"
    cat "$LOG"
    exit 1
fi

# Check log pattern if requested
if [[ -n "$LOG_PATTERN" ]]; then
    if grep -qE "$LOG_PATTERN" "$LOG"; then
        echo "=== PASS: log matched '$LOG_PATTERN' ==="
    else
        echo "=== FAIL: log did not match '$LOG_PATTERN' ==="
        echo "QEMU log:"
        cat "$LOG"
        exit 1
    fi
fi

# Remap isa-debug-exit codes
case $QEMU_RC in
    1)
        echo "=== PASS: guest exited successfully ==="
        exit 0
        ;;
    3)
        echo "=== FAIL: guest reported failure ==="
        echo "QEMU log:"
        cat "$LOG"
        exit 1
        ;;
    *)
        echo "=== FAIL: unexpected QEMU exit code $QEMU_RC ==="
        echo "QEMU log:"
        cat "$LOG"
        exit 1
        ;;
esac
