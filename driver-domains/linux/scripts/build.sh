#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Reproducible Buildroot wrapper for the RustOS Linux driver-domain appliance.

set -euo pipefail
IFS=$'\n\t'
export LC_ALL=C
export TZ=UTC

readonly ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
readonly LOCK_FILE="$ROOT/sources.lock"
readonly ADDITIVE_PACKAGE_CACHE_POLICY="$ROOT/scripts/additive-package-cache-v1.txt"
readonly COMMAND="${1:-build}"
readonly OUT_DIR="${OUT:-$ROOT/out}"
readonly JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}"
readonly DL_DIR="$OUT_DIR/dl"
readonly SRC_DIR="$OUT_DIR/src"
readonly BUILD_DIR="$OUT_DIR/buildroot-output"
readonly ARTIFACT_DIR="$OUT_DIR/artifacts"
readonly DEV_OUTPUT_MARKER="$BUILD_DIR/.rustos-dvm-dev-output-v1"
readonly LIBELF_SYSROOT="${RUSTOS_DVM_LIBELF_SYSROOT:-}"
readonly DVM_CCACHE_DIR="${RUSTOS_DVM_CCACHE_DIR:-$OUT_DIR/ccache}"
BUILDROOT_DIR=""
LIBELF_INCLUDE_DIR=""
LIBELF_LIBRARY_DIR=""
HOST_TOOL_DIR="$OUT_DIR/host-tools"
readonly BUILD_LOCK_FILE="$ROOT/.rustos-dvm-build.lock"
export CCACHE_DIR="$DVM_CCACHE_DIR"
# Buildroot's setlocalversion runs from the vendored source below this checkout.
# Do not let it climb into RustOS's worktree and refresh the unrelated parent
# Git index on every config probe. Package-local repositories remain visible.
export GIT_CEILING_DIRECTORIES="$(cd -- "$ROOT/../.." && pwd -P)"

die() {
    echo "rustos-linux-dvm: $*" >&2
    exit 1
}

validate_jobs() {
    case "$JOBS" in
        '' | *[!0-9]*) die "JOBS must be a positive integer: $JOBS" ;;
    esac
    test "$JOBS" -gt 0 || die "JOBS must be a positive integer: $JOBS"
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "required host tool not found: $1"
}

acquire_build_lock() {
    require_tool flock
    # Buildroot permits separate output directories, but this appliance has one
    # managed output tree. A second wrapper can otherwise remove the first
    # wrapper's configure probes during input-hash invalidation.
    exec 9>"$BUILD_LOCK_FILE"
    flock -n 9 || die "another RustOS Linux DVM build is already using $OUT_DIR"
}

prepare_host_tools() {
    local gnu_install system_install

    gnu_install="$(command -v gnuinstall || true)"
    if test -z "$gnu_install"; then
        system_install="$(command -v install || true)"
        if test -n "$system_install" && "$system_install" --version 2>&1 | grep -q '^install (GNU coreutils) '; then
            gnu_install="$system_install"
        fi
    fi
    test -n "$gnu_install" || die "missing GNU install; install gnu-coreutils"
    mkdir -p "$HOST_TOOL_DIR"
    ln -sfn "$gnu_install" "$HOST_TOOL_DIR/install"
    export PATH="$HOST_TOOL_DIR:$PATH"
}

require_kernel_build_headers() {
    local candidate

    if test -f /usr/include/libelf.h && test -f /usr/include/gelf.h; then
        LIBELF_INCLUDE_DIR=/usr/include
        return
    fi

    test -n "$LIBELF_SYSROOT" || die "missing libelf development headers; install the host libelf development package (Debian/Ubuntu: sudo apt install libelf-dev), or set RUSTOS_DVM_LIBELF_SYSROOT to an immutable extracted libelf-dev package"
    candidate="$LIBELF_SYSROOT/usr/include"
    if test ! -f "$candidate/libelf.h" || test ! -f "$candidate/gelf.h"; then
        die "RUSTOS_DVM_LIBELF_SYSROOT does not contain usr/include/libelf.h and usr/include/gelf.h: $LIBELF_SYSROOT"
    fi

    for candidate in \
        "$LIBELF_SYSROOT/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || true)" \
        "$LIBELF_SYSROOT/usr/lib/x86_64-linux-gnu" \
        "$LIBELF_SYSROOT/usr/lib"; do
        if test -f "$candidate/libelf.so"; then
            LIBELF_LIBRARY_DIR="$candidate"
            break
        fi
    done
    test -n "$LIBELF_LIBRARY_DIR" || die "RUSTOS_DVM_LIBELF_SYSROOT does not contain an unversioned libelf.so: $LIBELF_SYSROOT"

    LIBELF_INCLUDE_DIR="$LIBELF_SYSROOT/usr/include"
    export C_INCLUDE_PATH="$LIBELF_INCLUDE_DIR${C_INCLUDE_PATH:+:$C_INCLUDE_PATH}"
    export LIBRARY_PATH="$LIBELF_LIBRARY_DIR${LIBRARY_PATH:+:$LIBRARY_PATH}"
}

load_lock() {
    test -f "$LOCK_FILE" || die "missing source lock: $LOCK_FILE"
    # shellcheck source=/dev/null
    source "$LOCK_FILE"
    : "${BUILDROOT_VERSION:?} ${BUILDROOT_URL:?} ${BUILDROOT_SHA256:?}"
    : "${LINUX_VERSION:?} ${LINUX_URL:?} ${LINUX_SHA256:?}"
    BUILDROOT_DIR="$SRC_DIR/buildroot-${BUILDROOT_VERSION}"
}

sha256_ok() {
    local expected=$1
    local file=$2
    printf '%s  %s\n' "$expected" "$file" | sha256sum --check --status
}

fetch_one() {
    local url=$1
    local expected=$2
    local file=$3
    local tmp="${file}.partial"

    mkdir -p "$(dirname -- "$file")"
    if test -f "$file" && sha256_ok "$expected" "$file"; then
        return
    fi
    rm -f -- "$file"
    if test -f "$tmp"; then
        curl --fail --location --retry 3 --silent --show-error --continue-at - \
            --output "$tmp" "$url"
    else
        curl --fail --location --retry 3 --silent --show-error \
            --output "$tmp" "$url"
    fi
    sha256_ok "$expected" "$tmp" || die "checksum mismatch for $url"
    mv -- "$tmp" "$file"
}

prepare_sources() {
    local archive="$DL_DIR/buildroot-${BUILDROOT_VERSION}.tar.xz"
    local marker="$BUILDROOT_DIR/.rustos-buildroot-sha256"

    fetch_one "$BUILDROOT_URL" "$BUILDROOT_SHA256" "$archive"
    fetch_one "$LINUX_URL" "$LINUX_SHA256" "$DL_DIR/linux-${LINUX_VERSION}.tar.xz"

    if test -f "$marker" && test "$(cat "$marker")" = "$BUILDROOT_SHA256"; then
        return
    fi
    rm -rf -- "$BUILDROOT_DIR"
    mkdir -p "$SRC_DIR"
    tar -xJf "$archive" -C "$SRC_DIR"
    test -d "$BUILDROOT_DIR" || die "Buildroot archive did not create $BUILDROOT_DIR"
    printf '%s\n' "$BUILDROOT_SHA256" >"$marker"
}

config_input_hash() {
    (
        cd "$ROOT"
        # These inputs only describe the generated Buildroot configuration.
        # The complete BR2_* map is compared below before cache admission.
        find configs -type f -print0 | sort -z | xargs -0 sha256sum
        find package -name Config.in -type f -print0 | sort -z | xargs -0 sha256sum
        sha256sum external.desc external.mk Config.in
    ) | sha256sum | awk '{print $1}'
}

structural_config_input_hash() {
    (
        cd "$ROOT"
        # Only source/toolchain identities that cannot be reconciled from the
        # generated BR2_* map belong here. Linux, firmware, and external
        # device-class modules have narrower invalidation lanes below.
        printf '%s\n' "$BUILDROOT_VERSION" "$BUILDROOT_SHA256"
    ) | sha256sum | awk '{print $1}'
}

kernel_config_input_hash() {
    local fragment=${1:-$ROOT/board/linux.fragment}

    (
        cd "$ROOT"
        # Keep the kernel configuration and the host headers used by the
        # kernel build together.  This identity deliberately excludes the
        # target toolchain and userspace packages.
        sha256sum "$fragment" | awk '{print $1}'
        printf '%s\n' "$LINUX_VERSION" "$LINUX_SHA256"
        sha256sum "$LIBELF_INCLUDE_DIR/libelf.h" "$LIBELF_INCLUDE_DIR/gelf.h"
        if test -n "$LIBELF_LIBRARY_DIR"; then
            sha256sum "$LIBELF_LIBRARY_DIR/libelf.so"
        fi
    ) | sha256sum | awk '{print $1}'
}

render_desired_config() {
    local output=$1
    local probe
    local result

    probe="$(mktemp -d "$OUT_DIR/config-probe.XXXXXX")"
    result=0
    make -C "$BUILDROOT_DIR" O="$probe" \
        BR2_EXTERNAL="$ROOT" BR2_DL_DIR="$DL_DIR" BR2_LOCALVERSION= \
        rustos_linux_dvm_x86_64_defconfig >/dev/null || result=$?
    if test "$result" -eq 0; then
        make -C "$BUILDROOT_DIR" O="$probe" \
            BR2_EXTERNAL="$ROOT" BR2_DL_DIR="$DL_DIR" BR2_LOCALVERSION= \
            olddefconfig >/dev/null || result=$?
    fi
    if test "$result" -eq 0; then
        cp -- "$probe/.config" "$output" || result=$?
    fi
    rm -rf -- "$probe"
    return "$result"
}

config_change_preserves_host_toolchain() {
    local previous=$1
    local desired=$2

    awk '
        function record(line, side, key, value) {
            if (line ~ /^# BR2_[A-Z0-9_]+ is not set$/) {
                key = line
                sub(/^# /, "", key)
                sub(/ is not set$/, "", key)
                value = "n"
            } else if (line ~ /^BR2_[A-Z0-9_]+=/) {
                key = line
                sub(/=.*/, "", key)
                value = substr(line, length(key) + 2)
            } else {
                return
            }
            if (side == 0) {
                before[key] = value
            } else {
                after[key] = value
            }
        }
        FILENAME == ARGV[1] {
            if ($0 == "" || $0 ~ /^#/) next
            if ($0 !~ /^BR2_PACKAGE_[A-Z0-9_]+$/ || admitted[$0]) exit 2
            admitted[$0] = 1
            next
        }
        FILENAME == ARGV[2] { record($0, 0); next }
        FILENAME == ARGV[3] { record($0, 1); next }
        END {
            changes = 0
            for (key in before) {
                if (!(key in after)) exit 1
                if (before[key] != after[key]) {
                    changes++
                    if (key == "BR2_ROOTFS_POST_BUILD_SCRIPT" ||
                        key == "BR2_EXTERNAL_RUSTOS_LINUX_DVM_VERSION") {
                        continue
                    }
                    if ((key == "BR2_TARGET_ROOTFS_CPIO_XZ" &&
                         before[key] == "y" && after[key] == "n") ||
                        (key == "BR2_TARGET_ROOTFS_CPIO_ZSTD" &&
                         before[key] == "n" && after[key] == "y")) {
                        continue
                    }
                    if (!(key in admitted) || before[key] != "n" || after[key] != "y") {
                        exit 1
                    }
                }
            }
            for (key in after) {
                if (!(key in before)) {
                    changes++
                    if (key == "BR2_ROOTFS_POST_BUILD_SCRIPT" ||
                        key == "BR2_EXTERNAL_RUSTOS_LINUX_DVM_VERSION") continue
                    if (key == "BR2_TARGET_ROOTFS_CPIO_ZSTD" &&
                        after[key] == "y") continue
                    if (!(key in admitted) || after[key] != "y") exit 1
                }
            }
            exit(changes > 0 ? 0 : 1)
        }
    ' "$ADDITIVE_PACKAGE_CACHE_POLICY" "$previous" "$desired"
}

selftest_config_cache_policy() {
    local tmp
    local previous
    local desired
    local kernel_before
    local kernel_after
    local structural_before
    local structural_after

    tmp="$(mktemp -d /tmp/rustos-dvm-config-policy.XXXXXX)"
    previous="$tmp/previous"
    desired="$tmp/desired"
    printf '%s\n' \
        '# BR2_PACKAGE_ACPID is not set' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$previous"
    printf '%s\n' \
        'BR2_PACKAGE_ACPID=y' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$desired"
    config_change_preserves_host_toolchain "$previous" "$desired" \
        || die "additive package selection did not preserve the cache"
    if config_change_preserves_host_toolchain "$desired" "$previous"; then
        die "package removal incorrectly preserved the cache"
    fi
    printf '%s\n' \
        '# BR2_PACKAGE_ACPID is not set' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=n' >"$desired"
    if config_change_preserves_host_toolchain "$previous" "$desired"; then
        die "toolchain change incorrectly preserved the cache"
    fi
    printf '%s\n' \
        '# BR2_PACKAGE_ACPID is not set' \
        'BR2_PACKAGE_MESA3D_GALLIUM_DRIVERS="virgl radeonsi"' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$desired"
    if config_change_preserves_host_toolchain "$previous" "$desired"; then
        die "package value change incorrectly preserved the cache"
    fi
    printf '%s\n' \
        'BR2_ROOTFS_POST_BUILD_SCRIPT=""' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$previous"
    printf '%s\n' \
        'BR2_ROOTFS_POST_BUILD_SCRIPT="/sealed/post-build.sh"' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$desired"
    config_change_preserves_host_toolchain "$previous" "$desired" \
        || die "rootfs-only policy change did not preserve the cache"
    printf '%s\n' \
        'BR2_EXTERNAL_RUSTOS_LINUX_DVM_VERSION="-gold-dirty"' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$previous"
    printf '%s\n' \
        'BR2_EXTERNAL_RUSTOS_LINUX_DVM_VERSION="-gnew-dirty"' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$desired"
    config_change_preserves_host_toolchain "$previous" "$desired" \
        || die "external metadata version change did not preserve the cache"
    printf '%s\n' \
        'BR2_TARGET_ROOTFS_CPIO_XZ=y' \
        '# BR2_TARGET_ROOTFS_CPIO_ZSTD is not set' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$previous"
    printf '%s\n' \
        '# BR2_TARGET_ROOTFS_CPIO_XZ is not set' \
        'BR2_TARGET_ROOTFS_CPIO_ZSTD=y' \
        'BR2_TOOLCHAIN_BUILDROOT_MUSL=y' >"$desired"
    config_change_preserves_host_toolchain "$previous" "$desired" \
        || die "xz-to-zstd rootfs transition did not preserve the cache"
    if config_change_preserves_host_toolchain "$desired" "$previous"; then
        die "zstd-to-xz rootfs downgrade incorrectly preserved the cache"
    fi

    cp -- "$ROOT/board/linux.fragment" "$tmp/linux.fragment"
    kernel_before="$(kernel_config_input_hash "$tmp/linux.fragment")"
    structural_before="$(structural_config_input_hash)"
    printf '%s\n' '# cache-routing-selftest' >>"$tmp/linux.fragment"
    kernel_after="$(kernel_config_input_hash "$tmp/linux.fragment")"
    structural_after="$(structural_config_input_hash)"
    test "$kernel_before" != "$kernel_after" \
        || die "kernel fragment change did not invalidate the kernel lane"
    test "$structural_before" = "$structural_after" \
        || die "kernel fragment change invalidated the host toolchain lane"
    rm -rf -- "$tmp"
    printf 'rustos-linux-dvm: config cache policy selftest passed\n'
}

local_service_input_hash() {
    local service=$1

    case "$service" in
        rustos-dvm-agent|rustos-dvm-block|rustos-dvm-display|rustos-dvm-net) ;;
        *) die "unknown local DVM service: $service" ;;
    esac
    (
        cd "$ROOT"
        # Buildroot copies SITE_METHOD=local packages once. Keep a distinct
        # stamp per package so a relay edit never rebuilds its independent
        # DVM companions or the host toolchain.
        find "package/$service" -type f ! -name Config.in -print0 | sort -z | xargs -0 sha256sum
    ) | sha256sum | awk '{print $1}'
}

nvidia_module_input_hash() {
    (
        cd "$ROOT"
        find package/rustos-dvm-nvidia-open -type f ! -name Config.in -print0 |
            sort -z | xargs -0 sha256sum
        printf '%s\n' "$NVIDIA_OPEN_VERSION" "$NVIDIA_OPEN_SHA256"
    ) | sha256sum | awk '{print $1}'
}

overlay_input_hash() {
    (
        cd "$ROOT"
        # Post-build policy changes only the finalized rootfs. Keep it on the
        # image-regeneration lane rather than invalidating the host toolchain.
        sha256sum board/post-build.sh board/amdgpu-firmware-1002-1900.txt sources.lock
        find board/overlay -type f -print0 | sort -z | xargs -0 sha256sum
    ) | sha256sum | awk '{print $1}'
}

amdgpu_firmware_input_hash() {
    (
        cd "$ROOT"
        sha256sum board/amdgpu-firmware-1002-1900.txt
        LC_ALL=C grep '^AMDGPU_.*_SHA256=' sources.lock
        LC_ALL=C grep -E '^BR2_PACKAGE_LINUX_FIRMWARE_(AMDGPU|I915|XE)=y$' \
            configs/rustos_linux_dvm_x86_64_defconfig
    ) | sha256sum | awk '{print $1}'
}

overlay_file_manifest() {
    (
        cd "$ROOT/board/overlay"
        find . -type f -printf '%P\n' | LC_ALL=C sort
    )
}

overlay_path_is_safe() {
    case "$1" in
        ''|/*|*'/../'*|../*|*/..|*'//'*) return 1 ;;
        *) return 0 ;;
    esac
}

write_overlay_file_manifest() {
    local path=$1
    local tmp="${path}.tmp"

    mkdir -p "$(dirname -- "$path")"
    overlay_file_manifest >"$tmp"
    mv -- "$tmp" "$path"
}

sync_rootfs_overlay() {
    local previous="$BUILD_DIR/.rustos-overlay-files-v1"
    local current="$BUILD_DIR/.rustos-overlay-files-v1.current"
    local target="$BUILD_DIR/target"
    local path
    local relative

    # A freshly reconfigured Buildroot tree has no target root yet. There is
    # nothing to synchronize at this point; return success so the normal make
    # step can create it rather than aborting the first clean build.
    test -d "$target" || return 0
    overlay_file_manifest >"$current"

    if test -f "$previous"; then
        while IFS= read -r relative; do
            overlay_path_is_safe "$relative" || die "unsafe retired overlay path: $relative"
            rm -f -- "$target/$relative"
        done < <(comm -23 "$previous" "$current")
    else
        # One-time migration for output trees created before overlay ownership
        # was tracked. Restrict pruning to RustOS DVM init scripts so package
        # output and unrelated Buildroot files are never inferred as overlay.
        for path in "$target"/etc/init.d/S[0-9][0-9]rustos-dvm-*; do
            test -f "$path" || continue
            relative="etc/init.d/${path##*/}"
            if ! grep -Fqx -- "$relative" "$current"; then
                rm -f -- "$path"
            fi
        done
    fi

    # Buildroot's overlay copy is additive. Copy current files explicitly so
    # modifications are reflected even when no package target is rebuilt;
    # retired files were pruned above from an ownership manifest.
    cp -a "$ROOT/board/overlay/." "$target/"
    rm -f -- "$current"
}

write_stamp() {
    local path=$1
    local value=$2
    local tmp="${path}.tmp"

    mkdir -p "$(dirname -- "$path")"
    printf '%s\n' "$value" >"$tmp"
    mv -- "$tmp" "$path"
}

make_buildroot() {
    make -C "$BUILDROOT_DIR" O="$BUILD_DIR" \
        BR2_EXTERNAL="$ROOT" BR2_DL_DIR="$DL_DIR" BR2_LOCALVERSION= "$@"
}

invalidate_rootfs_image() {
    rm -f -- \
        "$BUILD_DIR/images/rootfs.cpio" \
        "$BUILD_DIR/images/rootfs.cpio.xz" \
        "$BUILD_DIR/images/rootfs.cpio.zst"
}

make_release_rootfs() {
    # Initramfs decompression is on the boot critical path. A fixed single
    # worker keeps the zstd frame reproducible across developer machines;
    # level 3 favors sub-five-second boot over a marginally smaller artifact.
    make_buildroot -j "$JOBS" \
        "ROOTFS_CPIO_COMPRESS_CMD=zstd -3 -q -f -T1 -c"
}

require_warm_dvm_configuration() {
    local config_stamp="$BUILD_DIR/.rustos-config-input-v3.sha256"
    local buildroot_marker="$BUILDROOT_DIR/.rustos-buildroot-sha256"
    local current_config

    # This is an inner-loop command.  It must not fetch, configure, clean, or
    # silently rebuild a host toolchain: those integration operations can turn
    # a small relay edit into an hour-long build.
    test -d "$BUILDROOT_DIR" || die "no cached Buildroot source; run make build once before dev-*"
    test -f "$buildroot_marker" && test "$(cat "$buildroot_marker")" = "$BUILDROOT_SHA256" \
        || die "cached Buildroot source differs from sources.lock; run make build"
    test -f "$BUILD_DIR/.config" && test -f "$config_stamp" \
        || die "no cached DVM configuration; run make build once before dev-*"
    require_kernel_build_headers
    current_config="$(config_input_hash)"
    test "$(cat "$config_stamp")" = "$current_config" \
        || die "DVM configuration or toolchain inputs changed; run make build, not dev-*"
    test -d "$BUILD_DIR/host/bin" \
        || die "no cached DVM host toolchain; run make build, not dev-*"
}

mark_dev_output_dirty() {
    local service=$1
    local tmp="${DEV_OUTPUT_MARKER}.tmp"

    mkdir -p "$(dirname -- "$DEV_OUTPUT_MARKER")"
    {
        printf 'format=rustos-dvm-dev-output-v1\n'
        printf 'service=%s\n' "$service"
        printf 'input-sha256=%s\n' "$(local_service_input_hash "$service")"
    } >"$tmp"
    mv -- "$tmp" "$DEV_OUTPUT_MARKER"
}

clear_dev_output_marker() {
    rm -f -- "$DEV_OUTPUT_MARKER"
}

assert_release_output_is_current() {
    if test -f "$DEV_OUTPUT_MARKER"; then
        die "DVM target contains a dev-* package build; run the matching rebuild-* target before release verification"
    fi
}

print_build_plan() {
    local config_stamp="$BUILD_DIR/.rustos-config-input-v3.sha256"
    local structural_stamp="$BUILD_DIR/.rustos-structural-config-input-v2.sha256"
    local kernel_stamp="$BUILD_DIR/.rustos-kernel-config-input-v1.sha256"
    local firmware_stamp="$BUILD_DIR/.rustos-amdgpu-firmware-input-v1.sha256"
    local nvidia_stamp="$BUILD_DIR/.rustos-nvidia-module-input-v1.sha256"
    local desired
    local service
    local service_stamp
    local config_lane=0
    local lane_count=0

    require_kernel_build_headers
    if ! test -d "$BUILDROOT_DIR" \
        || ! test -f "$BUILDROOT_DIR/.rustos-buildroot-sha256" \
        || test "$(cat "$BUILDROOT_DIR/.rustos-buildroot-sha256")" != "$BUILDROOT_SHA256" \
        || ! test -f "$BUILD_DIR/.config"; then
        printf 'mode=cold-full reason=missing-or-stale-buildroot-configuration\n'
        return
    fi
    if ! test -f "$structural_stamp" \
        || test "$(cat "$structural_stamp")" != "$(structural_config_input_hash)"; then
        printf 'mode=full-output reason=buildroot-or-conservative-driver-source-identity\n'
        return
    fi
    if ! test -f "$config_stamp" \
        || test "$(cat "$config_stamp")" != "$(config_input_hash)"; then
        desired="$(mktemp "$OUT_DIR/desired-config-plan.XXXXXX")"
        if ! render_desired_config "$desired"; then
            rm -f -- "$desired"
            printf 'mode=full-output reason=unable-to-render-desired-config\n'
            return
        fi
        if cmp -s "$BUILD_DIR/.config" "$desired"; then
            config_lane=1
        elif config_change_preserves_host_toolchain "$BUILD_DIR/.config" "$desired"; then
            config_lane=2
        else
            rm -f -- "$desired"
            printf 'mode=full-output reason=unsafe-buildroot-config-transition\n'
            return
        fi
        rm -f -- "$desired"
    fi

    printf 'mode=incremental\n'
    if test "$config_lane" -eq 1; then
        printf 'lane=config-metadata\n'
        lane_count=$((lane_count + 1))
    elif test "$config_lane" -eq 2; then
        printf 'lane=target-package-or-rootfs-config\n'
        lane_count=$((lane_count + 1))
    fi
    if ! test -f "$kernel_stamp" \
        || test "$(cat "$kernel_stamp")" != "$(kernel_config_input_hash)"; then
        printf 'lane=linux+signed-kernel-modules+rootfs\n'
        lane_count=$((lane_count + 1))
    fi
    for service in rustos-dvm-agent rustos-dvm-block rustos-dvm-display rustos-dvm-net; do
        service_stamp="$BUILD_DIR/.${service}-input-v1.sha256"
        if ! test -f "$service_stamp" \
            || test "$(cat "$service_stamp")" != "$(local_service_input_hash "$service")"; then
            printf 'lane=%s+rootfs\n' "$service"
            lane_count=$((lane_count + 1))
        fi
    done
    if ! test -f "$firmware_stamp" \
        || test "$(cat "$firmware_stamp")" != "$(amdgpu_firmware_input_hash)"; then
        printf 'lane=linux-firmware-reinstall+rootfs\n'
        lane_count=$((lane_count + 1))
    fi
    if ! test -f "$nvidia_stamp" \
        || test "$(cat "$nvidia_stamp")" != "$(nvidia_module_input_hash)"; then
        printf 'lane=rustos-dvm-nvidia-open+rootfs\n'
        lane_count=$((lane_count + 1))
    fi
    if ! test -f "$BUILD_DIR/.rustos-overlay-input.sha256" \
        || test "$(cat "$BUILD_DIR/.rustos-overlay-input.sha256")" != "$(overlay_input_hash)"; then
        printf 'lane=rootfs-overlay-or-policy\n'
        lane_count=$((lane_count + 1))
    fi
    if test "$lane_count" -eq 0; then
        printf 'lane=none\n'
    fi
}

configure() {
    local stamp="$BUILD_DIR/.rustos-config-input-v3.sha256"
    local structural_stamp="$BUILD_DIR/.rustos-structural-config-input-v2.sha256"
    local current
    local current_structural
    local desired

    require_kernel_build_headers
    current="$(config_input_hash)"
    current_structural="$(structural_config_input_hash)"

    if test -f "$BUILD_DIR/.config" && test -f "$stamp" \
        && test "$(cat "$stamp")" = "$current"; then
        # One-time migration for output created before structural/package-only
        # hashes were separated. The combined hash proves this exact config.
        if ! test -f "$structural_stamp"; then
            write_stamp "$structural_stamp" "$current_structural"
        fi
        return
    fi
    if test -f "$BUILD_DIR/.config" \
        && test -f "$structural_stamp" \
        && test "$(cat "$structural_stamp")" = "$current_structural"; then
        desired="$(mktemp "$OUT_DIR/desired-config.XXXXXX")"
        if render_desired_config "$desired"; then
            # Cache admission policy controls how configuration transitions are
            # classified; it is not an artifact or toolchain input.  A policy
            # update must therefore be able to reconcile an otherwise identical
            # generated configuration without deleting the output tree.
            if cmp -s "$BUILD_DIR/.config" "$desired"; then
                rm -f -- "$desired"
                write_stamp "$stamp" "$current"
                write_stamp "$structural_stamp" "$current_structural"
                printf 'rustos-linux-dvm: reconciled unchanged configuration without cleaning\n'
                return
            fi
            if config_change_preserves_host_toolchain "$BUILD_DIR/.config" "$desired"; then
                cp -- "$desired" "$BUILD_DIR/.config"
                rm -f -- "$desired"
                invalidate_rootfs_image
                make_buildroot olddefconfig >/dev/null
                write_stamp "$stamp" "$current"
                write_stamp "$structural_stamp" "$current_structural"
                printf 'rustos-linux-dvm: preserved host toolchain for target/rootfs-only config\n'
                return
            fi
        fi
        rm -f -- "$desired"
    fi
    # Package removal, changed option values, or any structural/toolchain input
    # can leave stale target files or an incompatible sysroot. Keep those
    # changes on Buildroot's conservative clean-output path.
    if test -f "$BUILD_DIR/.config"; then
        make_buildroot distclean
    fi
    mkdir -p "$BUILD_DIR"
    make_buildroot rustos_linux_dvm_x86_64_defconfig
    make_buildroot olddefconfig
    write_stamp "$stamp" "$current"
    write_stamp "$structural_stamp" "$current_structural"
}

prepare_mutable_inputs() {
    local kernel_stamp="$BUILD_DIR/.rustos-kernel-config-input-v1.sha256"
    local kernel_current
    local kernel_build="$BUILD_DIR/build/linux-${LINUX_VERSION}"
    local target_modules="$BUILD_DIR/target/lib/modules/${LINUX_VERSION}"
    local firmware_stamp="$BUILD_DIR/.rustos-amdgpu-firmware-input-v1.sha256"
    local firmware_current
    local nvidia_stamp="$BUILD_DIR/.rustos-nvidia-module-input-v1.sha256"
    local nvidia_current
    local overlay_stamp="$BUILD_DIR/.rustos-overlay-input.sha256"
    local overlay_files_stamp="$BUILD_DIR/.rustos-overlay-files-v1"
    local overlay_current
    local service
    local service_stamp
    local service_current

    kernel_current="$(kernel_config_input_hash)"
    firmware_current="$(amdgpu_firmware_input_hash)"
    nvidia_current="$(nvidia_module_input_hash)"
    overlay_current="$(overlay_input_hash)"

    # Linux Kconfig changes invalidate only Linux and the packages that produce
    # modules signed by that kernel build. Removing the old module
    # directory prevents a stale signed object from surviving target-finalize.
    # The NVIDIA package is rebuilt unchanged solely because its module must be
    # signed against the new kernel; no NVIDIA source or device state is edited.
    if ! test -f "$kernel_stamp" \
        || test "$(cat "$kernel_stamp")" != "$kernel_current"; then
        if test -d "$kernel_build" || test -d "$target_modules"; then
            make_buildroot linux-dirclean
            make_buildroot rustos-dvm-block-dirclean
            make_buildroot rustos-dvm-display-dirclean
            make_buildroot rustos-dvm-nvidia-open-dirclean
            rm -rf -- "$target_modules"
        fi
        invalidate_rootfs_image
    fi

    # SITE_METHOD=local is intentionally explicit: Buildroot does not watch
    # the external source tree after its first rsync. Remove only the local
    # service package directories so the next ordinary make recreates and
    # reinstalls them while retaining the verified host toolchain and kernel.
    for service in rustos-dvm-agent rustos-dvm-block rustos-dvm-display rustos-dvm-net; do
        service_stamp="$BUILD_DIR/.${service}-input-v1.sha256"
        service_current="$(local_service_input_hash "$service")"
        if test -f "$service_stamp" && test "$(cat "$service_stamp")" != "$service_current"; then
            make_buildroot "${service}-dirclean"
        fi
    done

    # The external NVIDIA display module is a leaf package built against the
    # already-selected kernel. Its source, version, or firmware update must not
    # discard the host toolchain or rebuild unrelated DVM services.
    if ! test -f "$nvidia_stamp" \
        || test "$(cat "$nvidia_stamp")" != "$nvidia_current"; then
        if test -d "$BUILD_DIR/build/rustos-dvm-nvidia-open-${NVIDIA_OPEN_VERSION}"; then
            make_buildroot rustos-dvm-nvidia-open-dirclean
        fi
        invalidate_rootfs_image
    fi

    # The post-build policy prunes linux-firmware in target/ to the sealed AMD
    # profile. If that profile later gains a required payload, regenerating the
    # rootfs alone cannot restore the file that was already deleted from the
    # mutable target tree. Reinstall only the cached linux-firmware package;
    # this restores its target files without rebuilding the package, kernel,
    # toolchain, Mesa, or LLVM, after which post-build prunes the exact profile.
    if ! test -f "$firmware_stamp" \
        || test "$(cat "$firmware_stamp")" != "$firmware_current"; then
        if find "$BUILD_DIR/build" -maxdepth 2 \
            -path '*/linux-firmware-*/.stamp_target_installed' -print -quit |
            grep -q .; then
            make_buildroot linux-firmware-reinstall
        fi
        invalidate_rootfs_image
    fi

    # An overlay change must produce a new initramfs even when no package
    # target changed. Deleting just the generated rootfs asks Buildroot to run
    # target-finalize and rebuild the image; it does not invalidate packages or
    # host tools.
    if ! test -f "$overlay_files_stamp" \
        || ! test -f "$overlay_stamp" \
        || test "$(cat "$overlay_stamp")" != "$overlay_current"; then
        sync_rootfs_overlay
        invalidate_rootfs_image
    fi
}

commit_mutable_input_stamps() {
    local service
    for service in rustos-dvm-agent rustos-dvm-block rustos-dvm-display rustos-dvm-net; do
        write_stamp "$BUILD_DIR/.${service}-input-v1.sha256" "$(local_service_input_hash "$service")"
    done
    write_stamp "$BUILD_DIR/.rustos-nvidia-module-input-v1.sha256" \
        "$(nvidia_module_input_hash)"
    write_stamp "$BUILD_DIR/.rustos-overlay-input.sha256" "$(overlay_input_hash)"
    write_stamp "$BUILD_DIR/.rustos-amdgpu-firmware-input-v1.sha256" \
        "$(amdgpu_firmware_input_hash)"
    write_stamp "$BUILD_DIR/.rustos-kernel-config-input-v1.sha256" "$(kernel_config_input_hash)"
    write_overlay_file_manifest "$BUILD_DIR/.rustos-overlay-files-v1"
}

verify_config() {
    "$ROOT/scripts/verify-config.sh" "$BUILD_DIR/.config"
}

verify_kernel_config() {
    local config="$BUILD_DIR/build/linux-${LINUX_VERSION}/.config"
    "$ROOT/scripts/verify-kernel-config.sh" "$config"
}

verify_dvm_bootstrap_order() {
    local init_dir="$BUILD_DIR/target/etc/init.d"
    local block_start="$init_dir/S12rustos-dvm-block"

    # Storage is independent of guest networking, display composition, and
    # control-agent startup. It must start immediately after Buildroot's
    # module-admission turn, while stale late-start names fail closed.
    test -f "$block_start" && test -x "$block_start" && test ! -L "$block_start" || \
        die "DVM storage bootstrap script is missing or symlinked"
    test ! -e "$init_dir/S47rustos-dvm-block" || \
        die "DVM storage relay must not wait behind guest networking"
}

verify_module_signatures() {
    local kernel_build="$BUILD_DIR/build/linux-${LINUX_VERSION}"
    "$ROOT/scripts/verify-module-signatures.sh" \
        "$BUILD_DIR/target" "$kernel_build" "$kernel_build/certs/signing_key.x509" \
        "$LOCK_FILE"
}

verify_release_artifacts() {
    "$ROOT/scripts/verify-release-artifacts.sh" "$ARTIFACT_DIR"
}

write_manifest() {
    "$ROOT/scripts/write-manifest.sh" \
        "$BUILD_DIR" "$ARTIFACT_DIR" "$LOCK_FILE"
}

build() {
    prepare_sources
    configure
    verify_config
    prepare_mutable_inputs
    make_release_rootfs
    verify_config
    verify_kernel_config
    verify_dvm_bootstrap_order
    verify_module_signatures
    write_manifest
    verify_release_artifacts
    commit_mutable_input_stamps
    clear_dev_output_marker
}

rebuild_service() {
    local service=$1

    case "$service" in
        rustos-dvm-agent|rustos-dvm-block|rustos-dvm-display|rustos-dvm-net) ;;
        *) die "unknown local DVM service: $service" ;;
    esac
    prepare_sources
    configure
    verify_config
    make_buildroot "${service}-dirclean"
        invalidate_rootfs_image
    make_release_rootfs
    verify_config
    verify_kernel_config
    verify_dvm_bootstrap_order
    verify_module_signatures
    write_manifest
    verify_release_artifacts
    commit_mutable_input_stamps
    clear_dev_output_marker
}

dev_build_service() {
    local service=$1
    local started_seconds=$SECONDS

    case "$service" in
        rustos-dvm-agent|rustos-dvm-block|rustos-dvm-display|rustos-dvm-net) ;;
        *) die "unknown local DVM service: $service" ;;
    esac
    require_warm_dvm_configuration
    # SITE_METHOD=local snapshots source during extraction. Removing only this
    # package refreshes that snapshot, compiles it, and installs it into
    # target/. It does not regenerate rootfs.cpio.zst or release artifacts.
    make_buildroot "${service}-dirclean"
    make_buildroot -j "$JOBS" "$service"
    mark_dev_output_dirty "$service"
    printf 'rustos-linux-dvm: dev package compile complete service=%s elapsed_s=%s release-artifacts=stale\n' \
        "$service" "$((SECONDS - started_seconds))"
}

print_artifacts() {
    test -d "$ARTIFACT_DIR" || die "no artifacts yet; run make build"
    find "$ARTIFACT_DIR" -maxdepth 1 -type f -printf '%f\n' | sort
}

clean() {
    test -d "$BUILDROOT_DIR" || exit 0
    make_buildroot clean
    rm -rf -- "$ARTIFACT_DIR"
}

distclean() {
    case "$OUT_DIR" in
        "$ROOT"/*) ;;
        *) die "refusing to remove OUT outside $ROOT: $OUT_DIR" ;;
    esac
    rm -rf -- "$OUT_DIR"
}

main() {
    validate_jobs
    load_lock
    acquire_build_lock
    prepare_host_tools
    case "$COMMAND" in
        fetch)
            require_tool curl
            require_tool sha256sum
            require_tool tar
            prepare_sources
            ;;
        configure)
            require_tool make
            prepare_sources
            configure
            verify_config
            ;;
        build)
            require_tool make
            require_tool curl
            require_tool sha256sum
            require_tool tar
            require_kernel_build_headers
            build
            ;;
        rebuild-agent)
            require_tool make
            require_tool curl
            require_tool sha256sum
            require_tool tar
            require_kernel_build_headers
            rebuild_service rustos-dvm-agent
            ;;
        rebuild-block)
            require_tool make
            require_tool curl
            require_tool sha256sum
            require_tool tar
            require_kernel_build_headers
            rebuild_service rustos-dvm-block
            ;;
        rebuild-display)
            require_tool make
            require_tool curl
            require_tool sha256sum
            require_tool tar
            require_kernel_build_headers
            rebuild_service rustos-dvm-display
            ;;
        rebuild-net)
            require_tool make
            require_tool curl
            require_tool sha256sum
            require_tool tar
            require_kernel_build_headers
            rebuild_service rustos-dvm-net
            ;;
        dev-agent)
            require_tool make
            dev_build_service rustos-dvm-agent
            ;;
        dev-block)
            require_tool make
            dev_build_service rustos-dvm-block
            ;;
        dev-display)
            require_tool make
            dev_build_service rustos-dvm-display
            ;;
        dev-net)
            require_tool make
            dev_build_service rustos-dvm-net
            ;;
        verify)
            assert_release_output_is_current
            verify_config
            verify_kernel_config
            verify_dvm_bootstrap_order
            verify_module_signatures
            verify_release_artifacts
            ;;
        print-artifacts)
            print_artifacts
            ;;
        selftest-config-cache)
            require_kernel_build_headers
            selftest_config_cache_policy
            ;;
        build-plan)
            print_build_plan
            ;;
        ccache-stats)
            require_tool make
            make_buildroot ccache-stats
            ;;
        profile-build)
            require_tool make
            make_buildroot graph-build
            ;;
        clean)
            require_tool make
            clean
            ;;
        distclean)
            distclean
            ;;
        *)
            die "unknown command: $COMMAND"
            ;;
    esac
}

main
