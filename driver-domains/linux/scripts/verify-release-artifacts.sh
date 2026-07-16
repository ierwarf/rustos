#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

artifact_dir=${1:?usage: verify-release-artifacts.sh ARTIFACT_DIR}
manifest="$artifact_dir/rustos-linux-dvm-x86_64.manifest"
test -f "$manifest" && test ! -L "$manifest" || {
    echo "rustos-linux-dvm: missing release manifest: $manifest" >&2
    exit 1
}

required_keys=(
    schema id architecture boot data-plane
    control-plane control-protocol control-state control-transport
    control-authentication control-capabilities control-contract-sha256
    buildroot_version linux_version nvidia-open-version nvidia-open-sha256
    nvidia-open-redistribute display-kernel-modules module-signing-enforced
    module-signing-cert-sha256 kernel_sha256 rootfs_sha256 config_sha256
    kernel-config-sha256 sources_lock_sha256
)
declare -A values=()
while IFS= read -r line; do
    [[ "$line" =~ ^[a-z0-9_-]+=[^[:space:]=]+$ ]] || {
        echo "rustos-linux-dvm: malformed strict manifest line" >&2
        exit 1
    }
    key=${line%%=*}
    value=${line#*=}
    test -n "$key" && test -n "$value" && test -z "${values[$key]+x}" || {
        echo "rustos-linux-dvm: empty or duplicate manifest key: $key" >&2
        exit 1
    }
    admitted=false
    for required in "${required_keys[@]}"; do
        if test "$key" = "$required"; then
            admitted=true
            break
        fi
    done
    test "$admitted" = true || {
        echo "rustos-linux-dvm: unknown manifest key: $key" >&2
        exit 1
    }
    values[$key]=$value
done <"$manifest"
test "${#values[@]}" -eq "${#required_keys[@]}" || {
    echo "rustos-linux-dvm: incomplete manifest schema" >&2
    exit 1
}

for assignment in \
    'schema=8' \
    'id=rustos-linux-dvm-x86_64' \
    'architecture=x86_64' \
    'boot=linux-bzimage+cpio-xz' \
    'data-plane=hostd-input-ring-msix' \
    'control-plane=agent-v1-control' \
    'control-protocol=agent-v1' \
    'control-state=control' \
    'control-transport=kvm-vsock' \
    'control-authentication=dvm-agent-hmac-sha256-v1' \
    'control-capabilities=health,device-inventory,driver-inventory,input-stream' \
    'buildroot_version=2026.05' \
    'linux_version=6.12.94' \
    'nvidia-open-version=580.173.02' \
    'nvidia-open-sha256=8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b' \
    'nvidia-open-redistribute=no' \
    'display-kernel-modules=i915,xe,amdgpu,nvidia-drm' \
    'module-signing-enforced=yes'; do
    key=${assignment%%=*}
    expected=${assignment#*=}
    test "${values[$key]}" = "$expected" || {
        echo "rustos-linux-dvm: unsupported manifest value: $key" >&2
        exit 1
    }
done

while IFS=: read -r key file; do
    path="$artifact_dir/$file"
    test -f "$path" && test ! -L "$path" || {
        echo "rustos-linux-dvm: release artifact missing or symlinked: $path" >&2
        exit 1
    }
    expected=${values[$key]}
    test "${#expected}" -eq 64 && [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || {
        echo "rustos-linux-dvm: invalid SHA-256 manifest value: $key" >&2
        exit 1
    }
    actual=$(sha256sum "$path" | awk '{print $1}')
    test "$actual" = "$expected" || {
        echo "rustos-linux-dvm: release artifact hash mismatch: $file" >&2
        exit 1
    }
done <<'EOF'
kernel_sha256:rustos-linux-dvm-x86_64.bzImage
rootfs_sha256:rustos-linux-dvm-x86_64.rootfs.cpio.xz
config_sha256:rustos-linux-dvm-x86_64.config
kernel-config-sha256:rustos-linux-dvm-x86_64.kernel.config
module-signing-cert-sha256:rustos-linux-dvm-x86_64.module-signing.x509
sources_lock_sha256:rustos-linux-dvm-x86_64.sources.lock
control-contract-sha256:rustos-linux-dvm-x86_64.control.env
EOF

"$(dirname -- "$0")/verify-kernel-config.sh" \
    "$artifact_dir/rustos-linux-dvm-x86_64.kernel.config"
openssl x509 -inform DER \
    -in "$artifact_dir/rustos-linux-dvm-x86_64.module-signing.x509" \
    -noout >/dev/null
control_keys=0
while IFS= read -r line; do
    case "$line" in
        ''|'#'*) continue ;;
    esac
    [[ "$line" =~ ^CONTROL_[A-Z_]+=[^[:space:]=]+$ ]] || {
        echo "rustos-linux-dvm: malformed packaged control-contract line" >&2
        exit 1
    }
    control_keys=$((control_keys + 1))
done <"$artifact_dir/rustos-linux-dvm-x86_64.control.env"
test "$control_keys" -eq 6 || {
    echo "rustos-linux-dvm: packaged control contract must contain exactly six keys" >&2
    exit 1
}
for required in \
    'CONTROL_SCHEMA=1' \
    'CONTROL_PROTOCOL=agent-v1' \
    'CONTROL_STATE=control' \
    'CONTROL_TRANSPORT=kvm-vsock' \
    'CONTROL_AUTHENTICATION=dvm-agent-hmac-sha256-v1' \
    'CONTROL_CAPABILITIES=health,device-inventory,driver-inventory,input-stream'; do
    grep -qx "$required" "$artifact_dir/rustos-linux-dvm-x86_64.control.env" || {
        echo "rustos-linux-dvm: packaged control contract lacks $required" >&2
        exit 1
    }
done

printf 'rustos-linux-dvm: verified self-contained schema-8 release artifacts\n'
