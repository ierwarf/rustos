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

require 'BR2_x86_64=y'
require 'BR2_LINUX_KERNEL=y'
require 'BR2_KERNEL_HEADERS_AS_KERNEL=y'
require 'BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_6_12=y'
require 'BR2_TARGET_ROOTFS_CPIO=y'
require 'BR2_PACKAGE_KMOD=y'
require 'BR2_PACKAGE_RUSTOS_DVM_AGENT=y'
