#!/usr/bin/env bash
set -euo pipefail

out_path=${1:?output path is required}
shift
source_paths=("$@")
linkage=${USER_ELF_LINKAGE:-static}

if [[ ${#source_paths[@]} -eq 0 ]]; then
    echo "at least one source path is required" >&2
    exit 1
fi

compiler=${USER_ELF_CC:-}
if [[ -z "${compiler}" ]]; then
    case "${linkage}" in
        static)
            if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
                compiler=x86_64-linux-musl-gcc
            elif command -v musl-gcc >/dev/null 2>&1; then
                compiler=musl-gcc
            else
                echo "musl toolchain not found; install x86_64-linux-musl-gcc or set USER_ELF_CC" >&2
                exit 1
            fi
            ;;
        dynamic)
            if command -v gcc >/dev/null 2>&1; then
                compiler=gcc
            elif command -v cc >/dev/null 2>&1; then
                compiler=cc
            else
                echo "glibc toolchain not found; install gcc or set USER_ELF_CC" >&2
                exit 1
            fi
            ;;
        *)
            echo "unsupported USER_ELF_LINKAGE: ${linkage}" >&2
            exit 1
            ;;
    esac
fi

mkdir -p "$(dirname "${out_path}")"

common_flags=(
    -fPIE
    -O2
    -Wall
    -Wextra
    -ffunction-sections
    -fdata-sections
    -Wl,--gc-sections
    -Wl,--build-id=none
    -Wl,-z,max-page-size=0x1000
    -Wl,-z,noexecstack
    -Wl,-z,relro
    -Wl,-z,now
)

case "${linkage}" in
    static)
        exec "${compiler}" \
            -static-pie \
            "${common_flags[@]}" \
            -o "${out_path}" \
            "${source_paths[@]}"
        ;;
    dynamic)
        exec "${compiler}" \
            -pie \
            "${common_flags[@]}" \
            -Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2 \
            -Wl,-rpath,/lib64 \
            -Wl,-rpath,/lib/x86_64-linux-gnu \
            -o "${out_path}" \
            "${source_paths[@]}"
        ;;
esac
