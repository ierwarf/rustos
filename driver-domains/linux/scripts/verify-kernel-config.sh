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

require_disabled() {
    ! grep -Eq "^${1}=(y|m)$" "$config" || {
        echo "rustos-linux-dvm: forbidden kernel fallback enabled: $1" >&2
        exit 1
    }
}

require_enabled CONFIG_MODULES
require_enabled CONFIG_MODVERSIONS
require_enabled CONFIG_MODULE_SIG
require_enabled CONFIG_MODULE_SIG_FORCE
require_enabled CONFIG_MODULE_SIG_ALL
require_enabled CONFIG_MODULE_SIG_SHA256
require_enabled CONFIG_MODULE_SIG_KEY_TYPE_RSA
grep -qx 'CONFIG_MODULE_SIG_HASH="sha256"' "$config" || {
    echo "rustos-linux-dvm: module signature hash is not pinned to sha256" >&2
    exit 1
}
grep -qx 'CONFIG_MODULE_SIG_KEY="certs/signing_key.pem"' "$config" || {
    echo "rustos-linux-dvm: module signing key path is not the image-private build key" >&2
    exit 1
}
require_enabled CONFIG_DEVTMPFS
require_enabled CONFIG_DEVTMPFS_MOUNT
require_enabled CONFIG_HIGH_RES_TIMERS
require_enabled CONFIG_PREEMPT_DYNAMIC
grep -qx 'CONFIG_HZ_1000=y' "$config" || {
    echo "rustos-linux-dvm: 1 kHz scheduler timer is required" >&2
    exit 1
}
require_enabled CONFIG_PCI
require_enabled CONFIG_PCI_MSI
require_enabled CONFIG_UIO
require_disabled CONFIG_UIO_PCI_GENERIC
require_enabled CONFIG_VIRTIO_PCI
require_enabled CONFIG_VIRTIO_NET
require_enabled CONFIG_VIRTIO_BLK
require_enabled CONFIG_VIRTIO_CONSOLE
require_enabled CONFIG_VIRTIO_INPUT
require_enabled CONFIG_MEMORY_HOTPLUG
require_enabled CONFIG_MEMORY_HOTREMOVE
require_enabled CONFIG_ZONE_DEVICE
require_enabled CONFIG_DMA_SHARED_BUFFER
require_enabled CONFIG_SYNC_FILE
require_enabled CONFIG_INPUT_UINPUT
require_enabled CONFIG_DRM
require_enabled CONFIG_DRM_VIRTIO_GPU
require_enabled CONFIG_DRM_I915
require_enabled CONFIG_DRM_XE
require_enabled CONFIG_DRM_AMDGPU
require_enabled CONFIG_DRM_AMD_DC
require_enabled CONFIG_DRM_FBDEV_EMULATION
require_enabled CONFIG_VIRTIO_VSOCKETS
require_disabled CONFIG_UDMABUF
require_disabled CONFIG_DMABUF_MOVE_NOTIFY
require_disabled CONFIG_DMABUF_HEAPS
require_disabled CONFIG_DRM_AMDGPU_USERPTR
require_disabled CONFIG_VFIO
require_disabled CONFIG_IOMMUFD
require_disabled CONFIG_VIRTIO_IOMMU
require_disabled CONFIG_DRM_RADEON
require_disabled CONFIG_DRM_VGEM
require_disabled CONFIG_DRM_VKMS
require_disabled CONFIG_DRM_SIMPLEDRM
require_disabled CONFIG_FW_LOADER_USER_HELPER
