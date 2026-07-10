#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

build_dir=${1:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
artifact_dir=${2:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
lock_file=${3:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
images="$build_dir/images"

# shellcheck source=/dev/null
source "$lock_file"

kernel="$images/bzImage"
rootfs="$images/rootfs.cpio.xz"
config="$build_dir/.config"
manifest="$artifact_dir/rustos-linux-dvm-x86_64.manifest"

for file in "$kernel" "$rootfs" "$config" "$lock_file"; do
    test -f "$file" || {
        echo "rustos-linux-dvm: expected build output missing: $file" >&2
        exit 1
    }
done

mkdir -p "$artifact_dir"
cp -- "$kernel" "$artifact_dir/rustos-linux-dvm-x86_64.bzImage"
cp -- "$rootfs" "$artifact_dir/rustos-linux-dvm-x86_64.rootfs.cpio.xz"
cp -- "$config" "$artifact_dir/rustos-linux-dvm-x86_64.config"

hash() {
    sha256sum "$1" | awk '{print $1}'
}

{
    echo 'schema=1'
    echo 'id=rustos-linux-dvm-x86_64'
    echo 'architecture=x86_64'
    echo 'boot=linux-bzimage+cpio-xz'
    echo 'data-plane=virtio'
    echo 'control-plane=agent-v1-not-connected'
    printf 'buildroot_version=%s\n' "$BUILDROOT_VERSION"
    printf 'linux_version=%s\n' "$LINUX_VERSION"
    printf 'kernel_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.bzImage")"
    printf 'rootfs_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.rootfs.cpio.xz")"
    printf 'config_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.config")"
    printf 'sources_lock_sha256=%s\n' "$(hash "$lock_file")"
} >"$manifest"
