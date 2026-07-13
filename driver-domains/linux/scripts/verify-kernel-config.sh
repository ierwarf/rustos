#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

config=${1:?usage: verify-kernel-config.sh PATH_TO_KERNEL_CONFIG}
test -f "$config" || {
    echo "rustos-linux-dvm: missing kernel configuration: $config" >&2
    exit 1
}

require_enabled() {
    grep -Eq "^${1}=(y|m)$" "$config" || {
        echo "rustos-linux-dvm: required kernel feature missing: $1" >&2
        exit 1
    }
}

require_enabled CONFIG_MODULES
require_enabled CONFIG_MODVERSIONS
require_enabled CONFIG_DEVTMPFS
require_enabled CONFIG_DEVTMPFS_MOUNT
require_enabled CONFIG_PCI
require_enabled CONFIG_VIRTIO_PCI
require_enabled CONFIG_VIRTIO_NET
require_enabled CONFIG_VIRTIO_BLK
require_enabled CONFIG_VIRTIO_CONSOLE
require_enabled CONFIG_VIRTIO_INPUT
require_enabled CONFIG_INPUT_UINPUT
require_enabled CONFIG_DRM
require_enabled CONFIG_DRM_VIRTIO_GPU
require_enabled CONFIG_DRM_FBDEV_EMULATION
require_enabled CONFIG_VIRTIO_VSOCKETS
