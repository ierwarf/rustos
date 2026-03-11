#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

USB_DEV="${USB_DEV:-/dev/sda1}"

if [[ ! -b "$USB_DEV" ]]; then
  echo "Error: block device not found: $USB_DEV" >&2
  exit 1
fi

if [[ ! -r "$USB_DEV" || ! -w "$USB_DEV" ]]; then
  echo "Error: insufficient permissions for $USB_DEV" >&2
  echo "Hint: run with sudo or adjust device permissions." >&2
  exit 1
fi

echo "

====================================
Starting QEMU (ATA passthrough: $USB_DEV)...
====================================

"

qemu-system-x86_64 \
  -bios OVMF.fd \
  -drive if=pflash,format=raw,readonly=on,file=OVMF.fd \
  -drive if=ide,index=0,media=disk,format=raw,file="$USB_DEV" \
  -net none \
  -m 2G \
  -monitor none \
  -debugcon stdio \
  -global isa-debugcon.iobase=0xe9 \
  -d int -D qemu_interrupt.log \
  "$@"

echo "

====================================
QEMU exited with code $?
====================================

"
