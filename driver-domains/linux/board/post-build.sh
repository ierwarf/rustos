#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

target_dir=${1:?usage: post-build.sh TARGET_DIR}
board_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
profile="$board_dir/amdgpu-firmware-1002-1900.txt"
firmware_dir="$target_dir/lib/firmware/amdgpu"
declare -A allowed=()

test -d "$firmware_dir" || {
    echo "rustos-linux-dvm: missing AMDGPU firmware directory" >&2
    exit 1
}

while IFS= read -r name; do
    case "$name" in
        ''|/*|*/*|.*) echo "rustos-linux-dvm: invalid AMDGPU firmware profile entry: $name" >&2; exit 1 ;;
    esac
    allowed["$name"]=1
done <"$profile"

while IFS= read -r -d '' path; do
    name=${path##*/}
    if test -d "$path"; then
        echo "rustos-linux-dvm: unexpected AMDGPU firmware subdirectory: $name" >&2
        exit 1
    fi
    if ! test -v 'allowed[$name]'; then
        rm -f -- "$path"
    fi
done < <(find "$firmware_dir" -mindepth 1 -maxdepth 1 -print0)

printf 'rustos-linux-dvm: pruned AMDGPU firmware to %s sealed file(s)\n' "${#allowed[@]}"
