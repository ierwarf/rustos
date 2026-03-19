#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BUILD_MIRROR_DIR="$(mktemp -d /tmp/rustos-qemu.XXXXXX)"
DEBUGCON_LOG=""
DEBUGCON_TAIL_PID=""
QEMU_PROFILE="${RUSTOS_QEMU_PROFILE:-default}"
QEMU_ACCEL="${RUSTOS_QEMU_ACCEL:-}"
VFIO_FORCE=false
AUTO_PHOENIX3_PASSTHROUGH=false
QEMU_PROFILE_ARGS=()
QEMU_VFIO_ARGS=()
QEMU_USER_ARGS=()
VFIO_HOSTS=()

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

usage() {
  cat <<'EOF'
usage: ./run.sh [options] [qemu args...]

options:
  -profile, --profile <name>         qemu profile (default, g14)
  -accel-profile, --accel-profile <name>
                                     accelerator profile; use "kvm" for host CPU
  --vfio-pci <0000:bb:dd.f>          attach a vfio-pci host device to qemu (repeatable)
  --phoenix3-passthrough             auto-attach host Phoenix3 VGA function and same-slot audio
  --vfio-force                       allow devices that currently drive an active host display
  -h, --help                         show this help
EOF
}

append_unique_vfio_host() {
  local candidate="${1:-}"
  local existing

  if [[ -z "$candidate" ]]; then
    return
  fi

  for existing in "${VFIO_HOSTS[@]}"; do
    if [[ "$existing" == "$candidate" ]]; then
      return
    fi
  done

  VFIO_HOSTS+=("$candidate")
}

detect_phoenix3_devices() {
  local devpath
  local bdf
  local slot_prefix
  local sibling

  for devpath in /sys/bus/pci/devices/*; do
    [[ -f "$devpath/vendor" ]] || continue
    [[ -f "$devpath/device" ]] || continue
    if [[ "$(cat "$devpath/vendor")" != "0x1002" ]]; then
      continue
    fi
    if [[ "$(cat "$devpath/device")" != "0x1900" ]]; then
      continue
    fi

    bdf="$(basename "$devpath")"
    echo "$bdf"
    slot_prefix="${bdf%.*}"
    for sibling in /sys/bus/pci/devices/"$slot_prefix".*; do
      [[ -e "$sibling" ]] || continue
      [[ "$(basename "$sibling")" == "$bdf" ]] && continue
      if [[ -f "$sibling/class" ]] && [[ "$(cat "$sibling/class")" == 0x040300 ]]; then
        echo "$(basename "$sibling")"
      fi
    done
    return 0
  done

  echo "Phoenix3 (1002:1900) host GPU not found." >&2
  return 1
}

profile_has_machine_arg() {
  local arg
  for arg in "${QEMU_PROFILE_ARGS[@]}"; do
    [[ "$arg" == "-machine" ]] && return 0
  done
  return 1
}

ensure_vfio_available() {
  if [[ ! -c /dev/vfio/vfio && ! -c /dev/vfio ]]; then
    echo "VFIO is not available: /dev/vfio is missing." >&2
    exit 1
  fi
}

device_class_code() {
  local bdf="$1"
  cat "/sys/bus/pci/devices/$bdf/class"
}

device_drives_active_host_display() {
  local bdf="$1"
  local drm_node
  local enabled_path
  local status_path

  for drm_node in /sys/class/drm/card*-*; do
    [[ -e "$drm_node/device" ]] || continue
    if [[ "$(basename "$(readlink -f "$drm_node/device")")" != "$bdf" ]]; then
      continue
    fi

    enabled_path="$drm_node/enabled"
    status_path="$drm_node/status"

    if [[ -f "$enabled_path" ]] && [[ "$(cat "$enabled_path")" == "enabled" ]]; then
      return 0
    fi
    if [[ -f "$status_path" ]] && [[ "$(cat "$status_path")" == "connected" ]]; then
      return 0
    fi
  done

  return 1
}

validate_vfio_device() {
  local bdf="$1"
  local devpath="/sys/bus/pci/devices/$bdf"
  local driver_path
  local driver_name
  local iommu_group

  if [[ ! -d "$devpath" ]]; then
    echo "VFIO host device not found: $bdf" >&2
    exit 1
  fi

  if [[ ! -L "$devpath/driver" ]]; then
    echo "VFIO host device has no bound driver: $bdf" >&2
    exit 1
  fi

  driver_path="$(readlink -f "$devpath/driver")"
  driver_name="$(basename "$driver_path")"
  if [[ "$driver_name" != "vfio-pci" ]]; then
    echo "VFIO host device is not bound to vfio-pci: $bdf (current: $driver_name)" >&2
    exit 1
  fi

  if [[ -L "$devpath/iommu_group" ]]; then
    iommu_group="$(basename "$(readlink -f "$devpath/iommu_group")")"
    if [[ ! -e "/dev/vfio/$iommu_group" ]]; then
      echo "VFIO IOMMU group device is missing: /dev/vfio/$iommu_group for $bdf" >&2
      exit 1
    fi
  fi

  if [[ "$VFIO_FORCE" != true ]] && device_drives_active_host_display "$bdf"; then
    echo "Refusing to passthrough active host display device $bdf." >&2
    echo "Use --vfio-force only after moving the host off this GPU." >&2
    exit 1
  fi
}

configure_vfio_args() {
  local bdf
  local first_gpu=true
  local class_code

  ensure_vfio_available

  for bdf in "${VFIO_HOSTS[@]}"; do
    validate_vfio_device "$bdf"
    class_code="$(device_class_code "$bdf")"

    if [[ "$class_code" == 0x03* ]]; then
      if [[ "$first_gpu" == true ]]; then
        QEMU_VFIO_ARGS+=(
          -display none
          -vga none
          -device "vfio-pci,host=$bdf,multifunction=on,x-vga=on"
        )
        first_gpu=false
      else
        QEMU_VFIO_ARGS+=(-device "vfio-pci,host=$bdf")
      fi
    else
      QEMU_VFIO_ARGS+=(-device "vfio-pci,host=$bdf")
    fi
  done
}

build_profile_args() {
  case "$QEMU_PROFILE" in
    default)
      local machine_arg=""
      local cpu_arg=""
      if [[ "$QEMU_ACCEL" == "kvm" ]]; then
        machine_arg="q35,accel=kvm"
        cpu_arg="host"
      fi

      QEMU_PROFILE_ARGS=(
        -drive file=fat:rw:"$BUILD_MIRROR_DIR",format=raw
        -m 2G
      )
      if [[ -n "$machine_arg" ]]; then
        QEMU_PROFILE_ARGS+=(-machine "$machine_arg")
      fi
      if [[ -n "$cpu_arg" ]]; then
        QEMU_PROFILE_ARGS+=(-cpu "$cpu_arg")
      fi
      ;;
    g14)
      local machine_arg="q35"
      local cpu_arg="EPYC-v4"
      if [[ "$QEMU_ACCEL" == "kvm" ]]; then
        machine_arg="${machine_arg},accel=kvm"
        cpu_arg="host"
      fi

      QEMU_PROFILE_ARGS=(
        -drive file=fat:rw:"$BUILD_MIRROR_DIR",format=raw
        -machine "$machine_arg"
        -cpu "$cpu_arg"
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
}

configure_debugcon() {
  local use_debugcon_file=false
  local expect_stdio_target=""
  local arg

  for arg in "$@"; do
    if [[ -n "$expect_stdio_target" ]]; then
      case "$arg" in
        stdio|mon:stdio)
          use_debugcon_file=true
          ;;
      esac
      expect_stdio_target=""
      continue
    fi

    case "$arg" in
      -nographic)
        use_debugcon_file=true
        ;;
      -serial|-monitor)
        expect_stdio_target=1
        ;;
    esac
  done

  DEBUGCON_ARGS=(-debugcon stdio)
  if [[ "$use_debugcon_file" == true ]]; then
    DEBUGCON_LOG="$(mktemp /tmp/rustos-debugcon.XXXXXX.log)"
    : > "$DEBUGCON_LOG"
    tail -f "$DEBUGCON_LOG" &
    DEBUGCON_TAIL_PID=$!
    DEBUGCON_ARGS=(-debugcon "file:$DEBUGCON_LOG")
    echo "debugcon redirected to $DEBUGCON_LOG because stdio is already in use."
  fi
}

main() {
  local bdf

  trap cleanup EXIT

  # QEMU's vvfat backend touches host-side file metadata when it is pointed at
  # the boot image directly, so run from an isolated mirror instead.
  cp -a build/image/. "$BUILD_MIRROR_DIR/"

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
      --vfio-pci)
        VFIO_HOSTS+=("${2:-}")
        shift 2
        ;;
      --phoenix3-passthrough)
        AUTO_PHOENIX3_PASSTHROUGH=true
        shift
        ;;
      --vfio-force)
        VFIO_FORCE=true
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        QEMU_USER_ARGS+=("$1")
        shift
        ;;
    esac
  done

  set -- "${QEMU_USER_ARGS[@]}"

  build_profile_args

  if [[ "$AUTO_PHOENIX3_PASSTHROUGH" == true ]]; then
    while IFS= read -r bdf; do
      append_unique_vfio_host "$bdf"
    done < <(detect_phoenix3_devices)
  fi

  if (( ${#VFIO_HOSTS[@]} > 0 )); then
    if ! profile_has_machine_arg; then
      QEMU_PROFILE_ARGS=(-machine q35 "${QEMU_PROFILE_ARGS[@]}")
    fi
    configure_vfio_args
  fi

  configure_debugcon "$@"

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
    "${QEMU_VFIO_ARGS[@]}" \
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
}

main "$@"
