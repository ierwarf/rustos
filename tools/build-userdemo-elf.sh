#!/usr/bin/env bash
set -euo pipefail

out_path=${1:?output path is required}
source_path=${2:?source path is required}

compiler=${USER_ELF_CC:-}
if [[ -z "${compiler}" ]]; then
    if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
        compiler=x86_64-linux-musl-gcc
    elif command -v musl-gcc >/dev/null 2>&1; then
        compiler=musl-gcc
    else
        echo "musl toolchain not found; install x86_64-linux-musl-gcc or set USER_ELF_CC" >&2
        exit 1
    fi
fi

mkdir -p "$(dirname "${out_path}")"

exec "${compiler}" \
    -static-pie \
    -fPIE \
    -O2 \
    -Wall \
    -Wextra \
    -ffunction-sections \
    -fdata-sections \
    -Wl,--gc-sections \
    -Wl,--build-id=none \
    -Wl,-z,max-page-size=0x1000 \
    -Wl,-z,noexecstack \
    -Wl,-z,relro \
    -Wl,-z,now \
    -o "${out_path}" \
    "${source_path}"
