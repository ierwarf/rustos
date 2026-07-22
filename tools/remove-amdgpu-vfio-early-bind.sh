#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

CONF=/etc/modprobe.d/rustos-amd-vfio.conf
MODULES=/etc/initramfs-tools/modules
APPLY=0

usage() {
    cat <<'EOF'
usage: tools/remove-amdgpu-vfio-early-bind.sh [--apply]

Plan or remove the persistent RustOS AMD early-vfio configuration. The default
is read-only. --apply removes only the exact RustOS modprobe file and exact
vfio_pci initramfs module line, then runs update-initramfs. It never performs a
live unbind, reset, reboot, or poweroff.
EOF
}

die() {
    printf 'remove-amdgpu-vfio-early-bind: %s\n' "$*" >&2
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

for tool in awk grep install mktemp readlink update-initramfs; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool missing: $tool"
done
test -f "$MODULES" && test ! -L "$MODULES" ||
    die "$MODULES must be a regular non-symlink file"

expected_conf=$(printf '%s\n' \
    'options vfio-pci ids=1002:1900 disable_idle_d3=1' \
    'blacklist amdgpu')

remove_conf=0
if test -e "$CONF"; then
    test -f "$CONF" && test ! -L "$CONF" || die "$CONF is not a regular file"
    test "$(cat "$CONF")" = "$expected_conf" ||
        die "$CONF was modified; refusing to remove unknown policy"
    remove_conf=1
fi

module_lines=$(grep -xcF 'vfio_pci' "$MODULES" || true)
test "$module_lines" -le 1 || die "$MODULES contains duplicate vfio_pci lines"

shopt -s nullglob
for candidate in /etc/modprobe.d/*.conf; do
    test "$candidate" = "$CONF" && continue
    if grep -Eq '^[[:space:]]*(options[[:space:]]+vfio-pci.*ids=.*1002:1900|blacklist[[:space:]]+amdgpu)([[:space:]]|$)' "$candidate"; then
        die "another AMD early-bind blocker must be handled separately: $candidate"
    fi
done
if test -r /etc/default/grub &&
    grep -Eq '(vfio-pci\.ids=1002:1900|modprobe\.blacklist=amdgpu|rd\.driver\.blacklist=amdgpu)' /etc/default/grub; then
    die "GRUB also contains an AMD/VFIO override; remove that separate policy first"
fi
if test -r /proc/cmdline &&
    grep -Eq '(vfio-pci\.ids=1002:1900|modprobe\.blacklist=amdgpu|rd\.driver\.blacklist=amdgpu)' /proc/cmdline; then
    die "the active kernel command line also enforces AMD/VFIO; locate and remove its bootloader source first"
fi

current_driver=absent
for device in /sys/bus/pci/devices/????:??:??.?; do
    test -r "$device/vendor" && test -r "$device/device" || continue
    test "$(cat "$device/vendor")" = 0x1002 || continue
    test "$(cat "$device/device")" = 0x1900 || continue
    current_driver=unbound
    test ! -L "$device/driver" || current_driver=$(basename "$(readlink -f "$device/driver")")
done

printf '%s\n' \
    "remove-amdgpu-vfio-early-bind: plan" \
    "  current-driver=$current_driver" \
    "  remove-config=$remove_conf" \
    "  remove-vfio_pci-lines=$module_lines" \
    "  update-initramfs=$((remove_conf || module_lines))" \
    "  live-unbind=no live-reset=no automatic-reboot=no"

test "$APPLY" -eq 1 || {
    printf '  no changes made; rerun with --apply\n'
    exit 0
}

if test "$remove_conf" -eq 0 && test "$module_lines" -eq 0; then
    printf 'remove-amdgpu-vfio-early-bind: already removed; no changes made\n'
    exit 0
fi

if test "$(id -u)" -eq 0; then
    sudo_cmd=()
else
    command -v sudo >/dev/null 2>&1 || die "sudo is required for --apply"
    sudo -v
    sudo_cmd=(sudo)
fi

tmp_modules=$(mktemp)
trap 'rm -f -- "$tmp_modules"' EXIT
awk '$0 != "vfio_pci"' "$MODULES" >"$tmp_modules"

if test "$remove_conf" -eq 1; then
    "${sudo_cmd[@]}" rm -f -- "$CONF"
fi
if test "$module_lines" -eq 1; then
    "${sudo_cmd[@]}" install -o root -g root -m 0644 "$tmp_modules" "$MODULES"
fi
"${sudo_cmd[@]}" update-initramfs -u

test ! -e "$CONF" || die "$CONF still exists"
test "$(grep -xcF 'vfio_pci' "$MODULES" || true)" -eq 0 ||
    die "vfio_pci initramfs module line still exists"

printf '%s\n' \
    'remove-amdgpu-vfio-early-bind: removed' \
    '  the currently bound driver was not changed' \
    '  amdgpu can bind again only after the next cold boot'
