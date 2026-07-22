#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

CONF=/etc/modprobe.d/rustos-amd-vfio.conf
MODULES=/etc/initramfs-tools/modules
EXPECTED_VENDOR=0x1002
EXPECTED_DEVICE=0x1900
EXPECTED_SUBSYSTEM_VENDOR=0x1043
EXPECTED_SUBSYSTEM_DEVICE=0x3a48
APPLY=0

usage() {
    cat <<'EOF'
usage: tools/configure-amdgpu-vfio-early-bind.sh [--apply]

Plan or install the persistent GA403UM AMD 1002:1900 early-vfio configuration.
The default is read-only. --apply writes modprobe/initramfs configuration and
runs update-initramfs, but never unbinds, resets, reboots, or powers off.
EOF
}

die() {
    printf 'configure-amdgpu-vfio-early-bind: %s\n' "$*" >&2
    exit 1
}

while (($#)); do
    case "$1" in
        --apply) APPLY=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
    shift
done

for tool in awk grep install mktemp readlink stat update-initramfs; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool missing: $tool"
done
test -f "$MODULES" && test ! -L "$MODULES" ||
    die "$MODULES must be a regular non-symlink file"

expected_conf=$(printf '%s\n' \
    'options vfio-pci ids=1002:1900 disable_idle_d3=1' \
    'blacklist amdgpu')

if test -e "$CONF"; then
    test -f "$CONF" && test ! -L "$CONF" || die "$CONF is not a regular file"
    test "$(cat "$CONF")" = "$expected_conf" ||
        die "$CONF exists with different contents; refusing to overwrite it"
fi

shopt -s nullglob
for candidate in /etc/modprobe.d/*.conf; do
    test "$candidate" = "$CONF" && continue
    if grep -Eq '^[[:space:]]*(options[[:space:]]+vfio-pci.*ids=.*1002:1900|blacklist[[:space:]]+amdgpu)([[:space:]]|$)' "$candidate"; then
        die "conflicting persistent GPU policy exists in $candidate"
    fi
done

mapfile -t amd_displays < <(
    for device in /sys/bus/pci/devices/????:??:??.?; do
        test -r "$device/vendor" && test -r "$device/class" || continue
        test "$(cat "$device/vendor")" = "$EXPECTED_VENDOR" || continue
        case "$(cat "$device/class")" in 0x03*) printf '%s\n' "$device" ;; esac
    done
)
test "${#amd_displays[@]}" -eq 1 ||
    die "expected exactly one AMD display function, found ${#amd_displays[@]}"

device=${amd_displays[0]}
test "$(cat "$device/device")" = "$EXPECTED_DEVICE" || die "unexpected AMD device"
test "$(cat "$device/subsystem_vendor")" = "$EXPECTED_SUBSYSTEM_VENDOR" ||
    die "unexpected AMD subsystem vendor"
test "$(cat "$device/subsystem_device")" = "$EXPECTED_SUBSYSTEM_DEVICE" ||
    die "unexpected AMD subsystem device"

driver=unbound
if test -L "$device/driver"; then
    driver=$(basename "$(readlink -f "$device/driver")")
fi

module_lines=$(grep -xcF 'vfio_pci' "$MODULES" || true)
test "$module_lines" -le 1 || die "$MODULES contains duplicate vfio_pci lines"

printf '%s\n' \
    "configure-amdgpu-vfio-early-bind: plan" \
    "  target=$(basename "$device") driver=$driver boot_vga=$(cat "$device/boot_vga")" \
    "  install=$CONF" \
    "  ensure-module=vfio_pci" \
    "  update-initramfs=yes" \
    "  live-unbind=no live-reset=no automatic-reboot=no"

test "$APPLY" -eq 1 || {
    printf '  no changes made; rerun with --apply\n'
    exit 0
}

if test "$(id -u)" -eq 0; then
    sudo_cmd=()
else
    command -v sudo >/dev/null 2>&1 || die "sudo is required for --apply"
    sudo -v
    sudo_cmd=(sudo)
fi

tmp_conf=$(mktemp)
tmp_modules=$(mktemp)
trap 'rm -f -- "$tmp_conf" "$tmp_modules"' EXIT
printf '%s\n' "$expected_conf" >"$tmp_conf"
"${sudo_cmd[@]}" install -o root -g root -m 0644 "$tmp_conf" "$CONF"

if test "$module_lines" -eq 0; then
    awk '1; END { print "vfio_pci" }' "$MODULES" >"$tmp_modules"
    "${sudo_cmd[@]}" install -o root -g root -m 0644 "$tmp_modules" "$MODULES"
fi

"${sudo_cmd[@]}" update-initramfs -u
test "$(cat "$CONF")" = "$expected_conf" || die "installed modprobe policy mismatch"
test "$(grep -xcF 'vfio_pci' "$MODULES" || true)" -eq 1 ||
    die "vfio_pci initramfs module line was not installed exactly once"

printf '%s\n' \
    'configure-amdgpu-vfio-early-bind: installed' \
    '  the current driver was not changed' \
    '  the new binding policy takes effect only after the next cold boot'
