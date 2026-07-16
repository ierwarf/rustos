#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

target_dir=${1:?usage: verify-module-signatures.sh TARGET_DIR KERNEL_BUILD_DIR CERT SOURCES_LOCK}
kernel_build_dir=${2:?usage: verify-module-signatures.sh TARGET_DIR KERNEL_BUILD_DIR CERT SOURCES_LOCK}
certificate=${3:?usage: verify-module-signatures.sh TARGET_DIR KERNEL_BUILD_DIR CERT SOURCES_LOCK}
sources_lock=${4:?usage: verify-module-signatures.sh TARGET_DIR KERNEL_BUILD_DIR CERT SOURCES_LOCK}
extract="$kernel_build_dir/scripts/extract-module-sig.pl"
modules="$target_dir/lib/modules"
private_key="$kernel_build_dir/certs/signing_key.pem"

for tool in openssl perl; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "rustos-linux-dvm: required module-signature verifier missing: $tool" >&2
        exit 1
    }
done
for path in "$extract" "$certificate" "$private_key" "$modules" "$sources_lock"; do
    test -e "$path" || {
        echo "rustos-linux-dvm: module-signature input missing: $path" >&2
        exit 1
    }
done
source "$sources_lock"
if test -L "$private_key" || ! test -f "$private_key" \
    || test "$(stat -c '%u' "$private_key")" != "$(id -u)" \
    || test "$(stat -c '%a' "$private_key")" != 600; then
    echo "rustos-linux-dvm: module-signing private key must be an owner-held 0600 regular file" >&2
    exit 1
fi

tmp=$(mktemp -d)
cleanup() {
    rm -f -- "$tmp/module" "$tmp/signature" "$tmp/certificate.pem"
    rmdir -- "$tmp"
}
trap cleanup EXIT

openssl x509 -inform DER -in "$certificate" -out "$tmp/certificate.pem"
count=0
while IFS= read -r -d '' module; do
    descriptor=$(perl "$extract" -d "$module" 2>/dev/null) || {
        echo "rustos-linux-dvm: unsigned or malformed module: $module" >&2
        exit 1
    }
    read -r _ _ id_type _ _ signature_len <<<"$descriptor"
    if test "$id_type" != 2 || test "$signature_len" -le 0; then
        echo "rustos-linux-dvm: module does not carry PKCS#7: $module" >&2
        exit 1
    fi
    perl "$extract" -0 "$module" >"$tmp/module" 2>/dev/null
    perl "$extract" -s "$module" >"$tmp/signature" 2>/dev/null
    openssl cms -verify -binary -inform DER \
        -in "$tmp/signature" -content "$tmp/module" \
        -certfile "$tmp/certificate.pem" -CAfile "$tmp/certificate.pem" \
        -purpose any -out /dev/null 2>/dev/null || {
        echo "rustos-linux-dvm: module signature does not match the pinned certificate: $module" >&2
        exit 1
    }
    count=$((count + 1))
done < <(find "$modules" -type f -name '*.ko*' -print0)

test "$count" -gt 0 || {
    echo "rustos-linux-dvm: no installed kernel modules to verify" >&2
    exit 1
}
for required in nvidia nvidia-modeset nvidia-drm rustos_dvm_ivshmem_uio i915 xe amdgpu; do
    find "$modules" -type f -name "$required.ko*" -print -quit | grep -q . || {
        echo "rustos-linux-dvm: required signed display module missing: $required" >&2
        exit 1
    }
done
if find "$modules" -type f -name 'nvidia-uvm.ko*' -print -quit | grep -q .; then
    echo "rustos-linux-dvm: forbidden NVIDIA UVM/compute module installed" >&2
    exit 1
fi

# The current AMD production target (PCI 1002:1900, Phoenix/HawkPoint GC 11.0.1)
# must be bootable from the self-contained DVM image.  A Buildroot package
# selection is not supply evidence: fail the image build if any exact firmware
# consumed by this target is absent from the sealed rootfs.
amdgpu_firmware="$target_dir/lib/firmware/amdgpu"
amdgpu_required=(
    "gc_11_0_1_imu.bin:$AMDGPU_GC_11_0_1_IMU_SHA256"
    "gc_11_0_1_me.bin:$AMDGPU_GC_11_0_1_ME_SHA256"
    "gc_11_0_1_mec.bin:$AMDGPU_GC_11_0_1_MEC_SHA256"
    "gc_11_0_1_mes.bin:$AMDGPU_GC_11_0_1_MES_SHA256"
    "gc_11_0_1_mes1.bin:$AMDGPU_GC_11_0_1_MES1_SHA256"
    "gc_11_0_1_mes_2.bin:$AMDGPU_GC_11_0_1_MES_2_SHA256"
    "gc_11_0_1_pfp.bin:$AMDGPU_GC_11_0_1_PFP_SHA256"
    "gc_11_0_1_rlc.bin:$AMDGPU_GC_11_0_1_RLC_SHA256"
    "psp_13_0_4_ta.bin:$AMDGPU_PSP_13_0_4_TA_SHA256"
    "psp_13_0_4_toc.bin:$AMDGPU_PSP_13_0_4_TOC_SHA256"
    "sdma_6_0_1.bin:$AMDGPU_SDMA_6_0_1_SHA256"
    "vcn_4_0_2.bin:$AMDGPU_VCN_4_0_2_SHA256"
)
for entry in "${amdgpu_required[@]}"; do
    required=${entry%%:*}
    expected=${entry#*:}
    path="$amdgpu_firmware/$required"
    if test -L "$path" || ! test -f "$path"; then
        echo "rustos-linux-dvm: required regular AMDGPU firmware missing: $required" >&2
        exit 1
    fi
    actual=$(sha256sum "$path" | awk '{print $1}')
    test "$actual" = "$expected" || {
        echo "rustos-linux-dvm: AMDGPU firmware digest mismatch: $required" >&2
        exit 1
    }
done
printf 'rustos-linux-dvm: cryptographically verified %s signed module(s)\n' "$count"
