#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BDF=0000:65:00.0
EXPECTED_VENDOR=0x1002
EXPECTED_DEVICE=0x1900
EXPECTED_SUBSYSTEM_VENDOR=0x1043
EXPECTED_SUBSYSTEM_DEVICE=0x3a48
CHECK_ONLY=0
VFCT=

usage() {
    cat <<'EOF'
usage: tools/prepare-physical-amdgpu-vfio-lab.sh [--check] [AMD_VFCT]

Prepare only the exact GA403UM AMD 1002:1900 function for RustOS's
NON-COMMERCIAL physical-QEMU lab lane. The script never unbinds a driver,
resets a PCI function, or starts QEMU. Run it from the terminal that will
later launch `cargo xtask kvm-smoke` so that the raised memlock is inherited.

  --check  verify an already-prepared device without privileged mutations
EOF
}

die() {
    printf 'prepare-physical-amdgpu-vfio-lab: %s\n' "$*" >&2
    exit 1
}

while (($#)); do
    case "$1" in
        --check)
            CHECK_ONLY=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -* )
            usage >&2
            die "unknown option: $1"
            ;;
        *)
            test -z "$VFCT" || die "only one AMD VFCT path is accepted"
            VFCT=$1
            ;;
    esac
    shift
done

for tool in cargo find getfacl readlink setpci stat; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool missing: $tool"
done
if test "$CHECK_ONLY" -eq 0; then
    for tool in modprobe prlimit setfacl sudo tee; do
        command -v "$tool" >/dev/null 2>&1 || die "required tool missing: $tool"
    done
    test "$(id -u)" -ne 0 || die "run as the desktop user, not as root"
fi

device=/sys/bus/pci/devices/$BDF
test -d "$device" || die "PCI function is absent: $BDF"
test "$(cat "$device/vendor")" = "$EXPECTED_VENDOR" || die "unexpected PCI vendor"
test "$(cat "$device/device")" = "$EXPECTED_DEVICE" || die "unexpected PCI device"
test "$(cat "$device/subsystem_vendor")" = "$EXPECTED_SUBSYSTEM_VENDOR" ||
    die "unexpected PCI subsystem vendor"
test "$(cat "$device/subsystem_device")" = "$EXPECTED_SUBSYSTEM_DEVICE" ||
    die "unexpected PCI subsystem device"

group=$(readlink -f "$device/iommu_group")
test -d "$group" || die "IOMMU group is unavailable"
group_id=$(basename "$group")
mapfile -t group_members < <(
    find "$group/devices" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort
)
test "${#group_members[@]}" -eq 1 && test "${group_members[0]}" = "$BDF" ||
    die "IOMMU group $group_id is not isolated to $BDF"

driver=unbound
if test -L "$device/driver"; then
    driver=$(basename "$(readlink -f "$device/driver")")
fi
if test "$driver" != unbound && test "$driver" != vfio-pci; then
    die "refusing to unbind active driver $driver; detach the AMD GPU separately"
fi

# disable_idle_d3 is module-wide. Refuse to change it while any other function
# is attached to vfio-pci; this lab helper must never affect an NVIDIA device.
other_vfio=
if test -d /sys/bus/pci/drivers/vfio-pci; then
    for candidate in /sys/bus/pci/drivers/vfio-pci/????:??:??.?; do
        test -e "$candidate" || continue
        candidate=$(basename "$candidate")
        test "$candidate" = "$BDF" || other_vfio="$other_vfio $candidate"
    done
fi
test -z "$other_vfio" || die "another VFIO function is present:$other_vfio"

if test "$CHECK_ONLY" -eq 0; then
    parent_uid=$(awk '/^Uid:/{print $2}' "/proc/$PPID/status")
    test "$parent_uid" = "$(id -u)" ||
        die "parent process is not owned by the invoking desktop user"

    sudo -v
    sudo modprobe vfio-pci
    printf 'Y\n' | sudo tee /sys/module/vfio_pci/parameters/disable_idle_d3 >/dev/null

    # Linux disables reset methods when reset_method receives an empty name
    # list. This is also required after an early-boot vfio-pci bind, where the
    # kernel has restored its default bus-reset method. The write changes only
    # future reset admission; this helper never executes a reset or unbind.
    if test -n "$(cat "$device/reset_method")"; then
        printf '\n' | sudo tee "$device/reset_method" >/dev/null
        test -z "$(cat "$device/reset_method")" || die "failed to disable reset methods"
    fi

    if test "$driver" = unbound; then
        printf 'vfio-pci\n' | sudo tee "$device/driver_override" >/dev/null
        printf '%s\n' "$BDF" | sudo tee /sys/bus/pci/drivers_probe >/dev/null
    fi
fi

test -L "$device/driver" || die "AMD GPU is not bound"
test "$(basename "$(readlink -f "$device/driver")")" = vfio-pci ||
    die "AMD GPU is not bound to vfio-pci"
test -z "$(cat "$device/reset_method")" || die "PCI reset methods remain enabled"
test "$(cat /sys/module/vfio_pci/parameters/disable_idle_d3)" = Y ||
    die "vfio-pci.disable_idle_d3 is not Y"

if test "$CHECK_ONLY" -eq 0; then
    # Clear only PCI_COMMAND.MASTER. QEMU may enable it only after the function
    # is attached to its non-identity IOMMUFD address space.
    sudo setpci -s "${BDF#0000:}" COMMAND=0000:0004
fi
command_register=$(setpci -s "${BDF#0000:}" COMMAND)
(( (16#$command_register & 4) == 0 )) || die "PCI bus mastering remains enabled"

vfio_sysfs=$(find "$device/vfio-dev" -mindepth 1 -maxdepth 1 -type d \
    -name 'vfio*' -print -quit)
test -n "$vfio_sysfs" || die "per-device VFIO cdev is absent"
vfio_dev=$(basename "$vfio_sysfs")
vfio_node=/dev/vfio/devices/$vfio_dev
legacy_node=/dev/vfio/$group_id
test -c /dev/iommu || die "/dev/iommu is not a character device"
test -c "$vfio_node" || die "$vfio_node is not a character device"

if test "$CHECK_ONLY" -eq 0; then
    acl_nodes=(/dev/iommu "$vfio_node")
    test ! -c "$legacy_node" || acl_nodes+=("$legacy_node")
    sudo setfacl -m "u:$(id -un):rw" "${acl_nodes[@]}"

    # Raise both this process (for the dry-run) and its interactive parent (for
    # the later real QEMU command). This cannot survive closing that terminal.
    sudo prlimit --pid "$$" --memlock=unlimited:unlimited
    sudo prlimit --pid "$PPID" --memlock=unlimited:unlimited
fi

test -r /dev/iommu && test -w /dev/iommu || die "/dev/iommu is not directly rw"
test -r "$vfio_node" && test -w "$vfio_node" || die "$vfio_node is not directly rw"

if test -z "$VFCT"; then
    mapfile -t vfct_candidates < <(
        find "$ROOT/build/kvm" -mindepth 2 -maxdepth 2 -type f \
            -path '*/physical-amdgpu-vbios.*/amdgpu-vfct-guest8.bin' -print 2>/dev/null |
            LC_ALL=C sort
    )
    test "${#vfct_candidates[@]}" -eq 1 ||
        die "pass one exact relocated AMD VFCT path"
    VFCT=${vfct_candidates[0]}
fi
test -f "$VFCT" && test ! -L "$VFCT" || die "AMD VFCT must be a regular non-symlink file"
VFCT=$(readlink -f -- "$VFCT")

cd -- "$ROOT"
cargo run -q -p rustos-hostd -- probe-iommufd
cargo xtask kvm-smoke \
    --dry-run \
    --timeout 30 \
    --gui-dvm-surfaces \
    --physical-gpu "$BDF" \
    --gpu-firmware "$VFCT"

printf '%s\n' \
    "prepare-physical-amdgpu-vfio-lab: ready; QEMU was not started" \
    "  target=$BDF group=$group_id vfio=$vfio_dev boot_vga=$(cat "$device/boot_vga")" \
    "  vfct=$VFCT" \
    "  real launch must run from this same parent terminal"
