#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BUILD_MIRROR_DIR="$(mktemp -d /tmp/rustos-qemu.XXXXXX)"
DEBUGCON_LOG=""
DEBUGCON_TAIL_PID=""
QEMU_PROFILE="${RUSTOS_QEMU_PROFILE:-default}"
QEMU_ACCEL="${RUSTOS_QEMU_ACCEL:-}"

cleanup() {
  if [[ -n "$DEBUGCON_TAIL_PID" ]]; then
    kill "$DEBUGCON_TAIL_PID" 2>/dev/null || true
    wait "$DEBUGCON_TAIL_PID" 2>/dev/null || true
  fi
  if [[ -n "$DEBUGCON_LOG" ]]; then
    rm -f "$DEBUGCON_LOG"
  fi
  rm -rf "$BUILD_MIRROR_DIR"
}

trap cleanup EXIT

# QEMU's vvfat backend touches host-side file metadata when it is pointed at
# the boot image directly, so run from an isolated mirror instead.
cp -a build/image/. "$BUILD_MIRROR_DIR/"

QEMU_PROFILE_ARGS=()
QEMU_PASSTHRU_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -profile|--profile)
      QEMU_PROFILE="${2:-}"
      shift 2
      ;;
    -accel-profile|--accel-profile)
      QEMU_ACCEL="${2:-}"
      shift 2
      ;;
    *)
      QEMU_PASSTHRU_ARGS+=("$1")
      shift
      ;;
  esac
done

set -- "${QEMU_PASSTHRU_ARGS[@]}"

case "$QEMU_PROFILE" in
  default)
    QEMU_PROFILE_ARGS=(
      -drive file=fat:rw:"$BUILD_MIRROR_DIR",format=raw
      -m 2G
    )
    ;;
  g14)
    MACHINE_ARG="q35"
    CPU_ARG="EPYC-v4"
    if [[ "$QEMU_ACCEL" == "kvm" ]]; then
      MACHINE_ARG="${MACHINE_ARG},accel=kvm"
      CPU_ARG="host"
    fi

    QEMU_PROFILE_ARGS=(
      -drive file=fat:rw:"$BUILD_MIRROR_DIR",format=raw
      -machine "$MACHINE_ARG"
      -cpu "$CPU_ARG"
      -smp 8,sockets=1,cores=8,threads=1
      -m 8G
      -rtc base=localtime,clock=host
    )
    ;;
  *)
    echo "unknown RUSTOS_QEMU_PROFILE: $QEMU_PROFILE" >&2
    exit 1
    ;;
esac

USE_DEBUGCON_FILE=false
EXPECT_STDIO_TARGET=""
for arg in "$@"; do
  if [[ -n "$EXPECT_STDIO_TARGET" ]]; then
    case "$arg" in
      stdio|mon:stdio)
        USE_DEBUGCON_FILE=true
        ;;
    esac
    EXPECT_STDIO_TARGET=""
    continue
  fi

  case "$arg" in
    -nographic)
      USE_DEBUGCON_FILE=true
      ;;
    -serial|-monitor)
      EXPECT_STDIO_TARGET=1
      ;;
  esac
done

DEBUGCON_ARGS=(-debugcon stdio)
if [[ "$USE_DEBUGCON_FILE" == true ]]; then
  DEBUGCON_LOG="$(mktemp /tmp/rustos-debugcon.XXXXXX.log)"
  : > "$DEBUGCON_LOG"
  tail -f "$DEBUGCON_LOG" &
  DEBUGCON_TAIL_PID=$!
  DEBUGCON_ARGS=(-debugcon "file:$DEBUGCON_LOG")
  echo "debugcon redirected to $DEBUGCON_LOG because stdio is already in use."
fi

echo "

====================================
Starting QEMU...
====================================

"

set +e
qemu-system-x86_64 \
  -bios OVMF.fd \
  -drive if=pflash,format=raw,readonly=on,file=OVMF.fd \
  "${QEMU_PROFILE_ARGS[@]}" \
  -net none \
  -monitor none \
  "${DEBUGCON_ARGS[@]}" \
  -global isa-debugcon.iobase=0xe9 \
  "$@"
QEMU_EXIT_CODE=$?
set -e

echo "

====================================
QEMU exited with code $QEMU_EXIT_CODE
====================================

"

exit "$QEMU_EXIT_CODE"
