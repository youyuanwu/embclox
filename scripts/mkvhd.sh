#!/bin/bash
# scripts/mkvhd.sh — Build a bootable Limine VHD image (BIOS+UEFI)
#
# Prerequisites: mtools, fdisk, qemu-img, cc
#
# Usage:
#   scripts/mkvhd.sh <kernel> <limine-dir> <limine-conf> <output.vhd>
#
# Example:
#   scripts/mkvhd.sh \
#       target/x86_64-unknown-none/release/embclox-tulip-example \
#       build/_deps/limine-src \
#       examples-tulip/limine.conf \
#       build/tulip.vhd

set -euo pipefail

KERNEL="$1"
LIMINE_DIR="$2"
LIMINE_CONF="$3"
OUTPUT_VHD="$4"

RAW="${OUTPUT_VHD%.vhd}.raw"
LIMINE_TOOL="${OUTPUT_VHD%.vhd}-limine"

# Compile the limine CLI tool if needed
if [ ! -f "$LIMINE_TOOL" ] || [ "$LIMINE_DIR/limine.c" -nt "$LIMINE_TOOL" ]; then
    echo "Compiling Limine CLI tool..."
    cc -std=c99 -O2 -o "$LIMINE_TOOL" "$LIMINE_DIR/limine.c"
fi

echo "Creating 64MB raw disk image..."
dd if=/dev/zero of="$RAW" bs=1M count=64 2>/dev/null

# Create MBR partition table: one FAT32 (LBA) partition starting at 1MB
echo "Creating MBR partition table..."
echo -e "o\nn\np\n1\n2048\n\nt\nc\na\nw\n" | fdisk "$RAW" > /dev/null 2>&1

# Format partition as FAT32 (offset 1MB = @@1M for mtools)
echo "Formatting FAT32 partition..."
mformat -i "${RAW}@@1M" -F ::

# Copy boot files
echo "Copying boot files..."
mmd -i "${RAW}@@1M" ::/boot ::/boot/limine ::/EFI ::/EFI/BOOT
mcopy -i "${RAW}@@1M" "$KERNEL" ::/boot/kernel
mcopy -i "${RAW}@@1M" "$LIMINE_CONF" ::/boot/limine/limine.conf
mcopy -i "${RAW}@@1M" "$LIMINE_DIR/limine-bios.sys" ::/boot/limine/limine-bios.sys
mcopy -i "${RAW}@@1M" "$LIMINE_DIR/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI

# Install Limine BIOS bootloader to MBR
echo "Installing Limine BIOS bootloader..."
"$LIMINE_TOOL" bios-install "$RAW"

# Convert to VHD (fixed size, required by Azure).
# Azure requires the virtual size to be an exact MB multiple.
echo "Converting to VHD..."
RAW_BYTES=$(stat -c%s "$RAW")
# Round up to nearest MB
MB=$((1024*1024))
ALIGNED_BYTES=$(( (RAW_BYTES + MB - 1) / MB * MB ))
if [ "$RAW_BYTES" -ne "$ALIGNED_BYTES" ]; then
    truncate -s "$ALIGNED_BYTES" "$RAW"
fi
qemu-img convert -f raw -O vpc -o subformat=fixed,force_size "$RAW" "$OUTPUT_VHD"
rm -f "$RAW"

echo "VHD created: $OUTPUT_VHD ($(du -h "$OUTPUT_VHD" | cut -f1))"
