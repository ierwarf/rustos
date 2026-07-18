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
readonly JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}"
readonly DL_DIR="$OUT_DIR/dl"
readonly SRC_DIR="$OUT_DIR/src"
readonly BUILD_DIR="$OUT_DIR/buildroot-output"
readonly ARTIFACT_DIR="$OUT_DIR/artifacts"
readonly DEV_OUTPUT_MARKER="$BUILD_DIR/.rustos-dvm-dev-output-v1"
readonly LIBELF_SYSROOT="${RUSTOS_DVM_LIBELF_SYSROOT:-}"
BUILDROOT_DIR=""
LIBELF_INCLUDE_DIR=""
LIBELF_LIBRARY_DIR=""
HOST_TOOL_DIR="$OUT_DIR/host-tools"
readonly BUILD_LOCK_FILE="$ROOT/.rustos-dvm-build.lock"

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

config_input_hash() {
    (
        cd "$ROOT"
        # These inputs can change the generated Buildroot configuration or
        # target-toolchain ABI. A change here deliberately starts from a clean
        # output tree because Buildroot cannot, in general, reconcile it with
        # already-built packages.
        find configs -type f -print0 | sort -z | xargs -0 sha256sum
        find package -name Config.in -type f -print0 | sort -z | xargs -0 sha256sum
        find package/rustos-dvm-nvidia-open -type f -print0 | sort -z | xargs -0 sha256sum
        sha256sum board/linux.fragment
        sha256sum sources.lock external.desc external.mk Config.in
        sha256sum "$LIBELF_INCLUDE_DIR/libelf.h" "$LIBELF_INCLUDE_DIR/gelf.h"
        if test -n "$LIBELF_LIBRARY_DIR"; then
            sha256sum "$LIBELF_LIBRARY_DIR/libelf.so"
        fi
    ) | sha256sum | awk '{print $1}'
}

local_service_input_hash() {
    local service=$1

    case "$service" in
        rustos-dvm-agent|rustos-dvm-display|rustos-dvm-net) ;;
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

overlay_input_hash() {
    (
        cd "$ROOT"
        find board/overlay -type f -print0 | sort -z | xargs -0 sha256sum
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
        BR2_EXTERNAL="$ROOT" BR2_DL_DIR="$DL_DIR" "$@"
}

make_release_rootfs() {
    local xz_threads=$JOBS

    # Buildroot deliberately disables threaded xz for reproducible builds
    # because its default block size depends on compression settings and host
    # parallelism.  Fixing the block size makes the stream independent of the
    # worker count while retaining the existing .cpio.xz boot/artifact ABI.
    # +1 requests the multi-threaded stream format even on a one-job builder.
    if test "$xz_threads" -eq 1; then
        xz_threads=+1
    fi
    make_buildroot -j "$JOBS" \
        "ROOTFS_CPIO_COMPRESS_CMD=xz -T $xz_threads --block-size=4MiB --memlimit-compress=70% -1 -C crc32 -c"
}

require_warm_dvm_configuration() {
    local config_stamp="$BUILD_DIR/.rustos-config-input-v2.sha256"
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

configure() {
    local stamp="$BUILD_DIR/.rustos-config-input-v2.sha256"
    local current

    require_kernel_build_headers
    current="$(config_input_hash)"

    if test -f "$BUILD_DIR/.config" && test -f "$stamp" \
        && test "$(cat "$stamp")" = "$current"; then
        return
    fi
    # Buildroot cannot safely reconcile an external configuration or toolchain
    # input change with already-built packages. Local package sources and the
    # rootfs overlay are handled below with targeted invalidation; they must
    # never force a host-toolchain rebuild.
    if test -f "$BUILD_DIR/.config"; then
        make_buildroot distclean
    fi
    mkdir -p "$BUILD_DIR"
    make_buildroot rustos_linux_dvm_x86_64_defconfig
    make_buildroot olddefconfig
    write_stamp "$stamp" "$current"
}

prepare_mutable_inputs() {
    local overlay_stamp="$BUILD_DIR/.rustos-overlay-input.sha256"
    local overlay_files_stamp="$BUILD_DIR/.rustos-overlay-files-v1"
    local overlay_current
    local service
    local service_stamp
    local service_current

    overlay_current="$(overlay_input_hash)"

    # SITE_METHOD=local is intentionally explicit: Buildroot does not watch
    # the external source tree after its first rsync. Remove only the local
    # service package directories so the next ordinary make recreates and
    # reinstalls them while retaining the verified host toolchain and kernel.
    for service in rustos-dvm-agent rustos-dvm-display rustos-dvm-net; do
        service_stamp="$BUILD_DIR/.${service}-input-v1.sha256"
        service_current="$(local_service_input_hash "$service")"
        if test -f "$service_stamp" && test "$(cat "$service_stamp")" != "$service_current"; then
            make_buildroot "${service}-dirclean"
        fi
    done

    # An overlay change must produce a new initramfs even when no package
    # target changed. Deleting just the generated rootfs asks Buildroot to run
    # target-finalize and rebuild the image; it does not invalidate packages or
    # host tools.
    if ! test -f "$overlay_files_stamp" \
        || ! test -f "$overlay_stamp" \
        || test "$(cat "$overlay_stamp")" != "$overlay_current"; then
        sync_rootfs_overlay
        rm -f -- "$BUILD_DIR/images/rootfs.cpio.xz"
    fi
}

commit_mutable_input_stamps() {
    local service
    for service in rustos-dvm-agent rustos-dvm-display rustos-dvm-net; do
        write_stamp "$BUILD_DIR/.${service}-input-v1.sha256" "$(local_service_input_hash "$service")"
    done
    write_stamp "$BUILD_DIR/.rustos-overlay-input.sha256" "$(overlay_input_hash)"
    write_overlay_file_manifest "$BUILD_DIR/.rustos-overlay-files-v1"
}

verify_config() {
    "$ROOT/scripts/verify-config.sh" "$BUILD_DIR/.config"
}

verify_kernel_config() {
    local config="$BUILD_DIR/build/linux-${LINUX_VERSION}/.config"
    "$ROOT/scripts/verify-kernel-config.sh" "$config"
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
    verify_module_signatures
    write_manifest
    verify_release_artifacts
    commit_mutable_input_stamps
    clear_dev_output_marker
}

rebuild_service() {
    local service=$1

    case "$service" in
        rustos-dvm-agent|rustos-dvm-display|rustos-dvm-net) ;;
        *) die "unknown local DVM service: $service" ;;
    esac
    prepare_sources
    configure
    verify_config
    make_buildroot "${service}-dirclean"
    rm -f -- "$BUILD_DIR/images/rootfs.cpio.xz"
    make_release_rootfs
    verify_config
    verify_kernel_config
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
        rustos-dvm-agent|rustos-dvm-display|rustos-dvm-net) ;;
        *) die "unknown local DVM service: $service" ;;
    esac
    require_warm_dvm_configuration
    # SITE_METHOD=local snapshots source during extraction. Removing only this
    # package refreshes that snapshot, compiles it, and installs it into
    # target/. It does not regenerate rootfs.cpio.xz or release artifacts.
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
            verify_module_signatures
            verify_release_artifacts
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
