#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

build_dir=${1:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
artifact_dir=${2:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
lock_file=${3:?usage: write-manifest.sh BUILD_DIR ARTIFACT_DIR LOCK_FILE}
images="$build_dir/images"
root=$(cd "$(dirname "$0")/.." && pwd)
control_contract="$root/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"

# shellcheck source=/dev/null
source "$lock_file"
# shellcheck source=/dev/null
source "$control_contract"

kernel="$images/bzImage"
rootfs="$images/rootfs.cpio.xz"
config="$build_dir/.config"
kernel_config="$build_dir/build/linux-${LINUX_VERSION}/.config"
module_signing_cert="$build_dir/build/linux-${LINUX_VERSION}/certs/signing_key.x509"
manifest="$artifact_dir/rustos-linux-dvm-x86_64.manifest"
tmp=''
manifest_tmp=''
trap 'rm -f -- "${tmp:-}" "${manifest_tmp:-}"' EXIT

for file in "$kernel" "$rootfs" "$config" "$kernel_config" "$module_signing_cert" "$lock_file" "$control_contract"; do
    test -f "$file" || {
        echo "rustos-linux-dvm: expected build output missing: $file" >&2
        exit 1
    }
done

mkdir -p "$artifact_dir"
install_artifact() {
    local source=$1
    local destination=$2

    tmp=$(mktemp "$artifact_dir/.${destination##*/}.tmp.XXXXXX")
    install -m 0644 -- "$source" "$tmp"
    sync -f "$tmp"
    mv -f -- "$tmp" "$destination"
    tmp=''
}

install_artifact "$kernel" "$artifact_dir/rustos-linux-dvm-x86_64.bzImage"
install_artifact "$rootfs" "$artifact_dir/rustos-linux-dvm-x86_64.rootfs.cpio.xz"
install_artifact "$config" "$artifact_dir/rustos-linux-dvm-x86_64.config"
install_artifact "$kernel_config" "$artifact_dir/rustos-linux-dvm-x86_64.kernel.config"
install_artifact "$module_signing_cert" "$artifact_dir/rustos-linux-dvm-x86_64.module-signing.x509"
install_artifact "$lock_file" "$artifact_dir/rustos-linux-dvm-x86_64.sources.lock"
install_artifact "$control_contract" "$artifact_dir/rustos-linux-dvm-x86_64.control.env"

hash() {
    sha256sum "$1" | awk '{print $1}'
}

manifest_tmp=$(mktemp "$artifact_dir/.rustos-linux-dvm-x86_64.manifest.tmp.XXXXXX")
{
    echo 'schema=8'
    echo 'id=rustos-linux-dvm-x86_64'
    echo 'architecture=x86_64'
    echo 'boot=linux-bzimage+cpio-xz'
    echo 'data-plane=hostd-input-ring-msix'
    printf 'control-plane=%s-%s\n' "$CONTROL_PROTOCOL" "$CONTROL_STATE"
    printf 'control-protocol=%s\n' "$CONTROL_PROTOCOL"
    printf 'control-state=%s\n' "$CONTROL_STATE"
    printf 'control-transport=%s\n' "$CONTROL_TRANSPORT"
    printf 'control-authentication=%s\n' "$CONTROL_AUTHENTICATION"
    printf 'control-capabilities=%s\n' "$CONTROL_CAPABILITIES"
    printf 'control-contract-sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.control.env")"
    printf 'buildroot_version=%s\n' "$BUILDROOT_VERSION"
    printf 'linux_version=%s\n' "$LINUX_VERSION"
    printf 'nvidia-open-version=%s\n' "$NVIDIA_OPEN_VERSION"
    printf 'nvidia-open-sha256=%s\n' "$NVIDIA_OPEN_SHA256"
    echo 'nvidia-open-redistribute=no'
    echo 'display-kernel-modules=i915,xe,amdgpu,nvidia-drm'
    echo 'module-signing-enforced=yes'
    printf 'module-signing-cert-sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.module-signing.x509")"
    printf 'kernel_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.bzImage")"
    printf 'rootfs_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.rootfs.cpio.xz")"
    printf 'config_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.config")"
    printf 'kernel-config-sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.kernel.config")"
    printf 'sources_lock_sha256=%s\n' "$(hash "$artifact_dir/rustos-linux-dvm-x86_64.sources.lock")"
} >"$manifest_tmp"
chmod 0644 "$manifest_tmp"
sync -f "$manifest_tmp"
mv -f -- "$manifest_tmp" "$manifest"
manifest_tmp=''
sync -f "$artifact_dir"
