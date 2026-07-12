#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Reproducible Buildroot wrapper for the RustOS Linux driver-domain appliance.

set -euo pipefail
IFS=$'\n\t'
export LC_ALL=C
export TZ=UTC

readonly ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
readonly LOCK_FILE="$ROOT/sources.lock"
readonly COMMAND="${1:-build}"
readonly OUT_DIR="${OUT:-$ROOT/out}"
readonly JOBS="${JOBS:-1}"
readonly DL_DIR="$OUT_DIR/dl"
readonly SRC_DIR="$OUT_DIR/src"
readonly BUILD_DIR="$OUT_DIR/buildroot-output"
readonly ARTIFACT_DIR="$OUT_DIR/artifacts"
readonly LIBELF_SYSROOT="${RUSTOS_DVM_LIBELF_SYSROOT:-}"
BUILDROOT_DIR=""
LIBELF_INCLUDE_DIR=""
LIBELF_LIBRARY_DIR=""
HOST_TOOL_DIR="$OUT_DIR/host-tools"

die() {
    echo "rustos-linux-dvm: $*" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "required host tool not found: $1"
}

prepare_host_tools() {
    local gnu_install

    gnu_install="$(command -v gnuinstall || true)"
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

input_hash() {
    (
        cd "$ROOT"
        find configs board package scripts -type f -print0 | sort -z | xargs -0 sha256sum
        sha256sum sources.lock external.desc external.mk Config.in
        sha256sum "$LIBELF_INCLUDE_DIR/libelf.h" "$LIBELF_INCLUDE_DIR/gelf.h"
        if test -n "$LIBELF_LIBRARY_DIR"; then
            sha256sum "$LIBELF_LIBRARY_DIR/libelf.so"
        fi
    ) | sha256sum | awk '{print $1}'
}

make_buildroot() {
    make -C "$BUILDROOT_DIR" O="$BUILD_DIR" \
        BR2_EXTERNAL="$ROOT" BR2_DL_DIR="$DL_DIR" "$@"
}

configure() {
    local stamp="$BUILD_DIR/.rustos-config-input.sha256"
    local current

    require_kernel_build_headers
    current="$(input_hash)"

    if test -f "$BUILD_DIR/.config" && test -f "$stamp" \
        && test "$(cat "$stamp")" = "$current"; then
        return
    fi
    # Buildroot does not necessarily track edits to BR2_EXTERNAL local-package
    # sources through its package stamps.  The DVM image must never report a
    # manifest/control hash for an old agent binary, so any hashed input change
    # invalidates the target tree before reconfiguration.
    if test -f "$BUILD_DIR/.config"; then
        make_buildroot clean
    fi
    mkdir -p "$BUILD_DIR"
    make_buildroot rustos_linux_dvm_x86_64_defconfig
    make_buildroot olddefconfig
    printf '%s\n' "$current" >"$stamp"
}

verify_config() {
    "$ROOT/scripts/verify-config.sh" "$BUILD_DIR/.config"
}

verify_kernel_config() {
    local config="$BUILD_DIR/build/linux-${LINUX_VERSION}/.config"
    "$ROOT/scripts/verify-kernel-config.sh" "$config"
}

write_manifest() {
    "$ROOT/scripts/write-manifest.sh" \
        "$BUILD_DIR" "$ARTIFACT_DIR" "$LOCK_FILE"
}

build() {
    prepare_sources
    configure
    verify_config
    make_buildroot -j "$JOBS"
    verify_config
    verify_kernel_config
    write_manifest
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
    load_lock
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
        verify)
            verify_config
            test -f "$ARTIFACT_DIR/rustos-linux-dvm-x86_64.manifest"
            ;;
        print-artifacts)
            print_artifacts
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
