#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

config=${1:?usage: verify-config.sh PATH_TO_CONFIG}
test -f "$config" || {
    echo "rustos-linux-dvm: missing Buildroot configuration: $config" >&2
    exit 1
}

require() {
    grep -qx "$1" "$config" || {
        echo "rustos-linux-dvm: required configuration missing: $1" >&2
        exit 1
    }
}

reject_enabled() {
    if grep -qx "$1=y" "$config"; then
        echo "rustos-linux-dvm: forbidden configuration enabled: $1" >&2
        exit 1
    fi
}

require 'BR2_x86_64=y'
require 'BR2_CCACHE=y'
require 'BR2_CCACHE_USE_BASEDIR=y'
require 'BR2_LINUX_KERNEL=y'
require 'BR2_KERNEL_HEADERS_AS_KERNEL=y'
require 'BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_6_12=y'
require 'BR2_TARGET_ROOTFS_CPIO=y'
require 'BR2_TARGET_ROOTFS_CPIO_ZSTD=y'
reject_enabled 'BR2_TARGET_ROOTFS_CPIO_XZ'
require 'BR2_PACKAGE_KMOD=y'
require 'BR2_PACKAGE_LINUX_FIRMWARE=y'
require 'BR2_PACKAGE_LINUX_FIRMWARE_AMDGPU=y'
require 'BR2_PACKAGE_LINUX_FIRMWARE_I915=y'
require 'BR2_PACKAGE_LINUX_FIRMWARE_XE=y'
require 'BR2_INSTALL_LIBSTDCPP=y'
require 'BR2_PACKAGE_MESA3D=y'
require 'BR2_PACKAGE_MESA3D_LLVM=y'
require 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_RADEONSI=y'
require 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y'
require 'BR2_PACKAGE_MESA3D_GBM=y'
require 'BR2_PACKAGE_MESA3D_OPENGL_EGL=y'
require 'BR2_PACKAGE_MESA3D_OPENGL_ES=y'
reject_enabled 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_LLVMPIPE'
reject_enabled 'BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE'
reject_enabled 'BR2_PACKAGE_MESA3D_VULKAN_DRIVER_SWRAST'
require 'BR2_PACKAGE_RUSTOS_DVM_AGENT=y'
require 'BR2_PACKAGE_RUSTOS_DVM_BLOCK=y'
require 'BR2_PACKAGE_RUSTOS_DVM_DISPLAY=y'
require 'BR2_PACKAGE_RUSTOS_DVM_NVIDIA_OPEN=y'
