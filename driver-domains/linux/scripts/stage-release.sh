#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

artifact_dir=${1:?usage: stage-release.sh ARTIFACT_DIR ABSOLUTE_DESTINATION}
destination=${2:?usage: stage-release.sh ARTIFACT_DIR ABSOLUTE_DESTINATION}
case "$destination" in
    /*) ;;
    *) echo "rustos-linux-dvm: release destination must be absolute" >&2; exit 1 ;;
esac

parent_input=$(dirname -- "$destination")
name=$(basename -- "$destination")
case "$name" in
    ''|.|..) echo "rustos-linux-dvm: invalid release destination" >&2; exit 1 ;;
esac
test ! -e "$destination" && test ! -L "$destination" || {
    echo "rustos-linux-dvm: refusing to replace existing release: $destination" >&2
    exit 1
}

parent=$(realpath -e -- "$parent_input")
test "$parent" = "$(realpath -m -s -- "$parent_input")" || {
    echo "rustos-linux-dvm: release parent must not traverse a symlink: $parent_input" >&2
    exit 1
}

owner=$(id -u)
directory=$parent
while :; do
    test -d "$directory" && test ! -L "$directory" || {
        echo "rustos-linux-dvm: untrusted release ancestor: $directory" >&2
        exit 1
    }
    directory_owner=$(stat -c '%u' "$directory")
    directory_mode=$(stat -c '%a' "$directory")
    if { test "$directory_owner" != 0 && test "$directory_owner" != "$owner"; } \
        || (( (8#$directory_mode & 8#022) != 0 )); then
        echo "rustos-linux-dvm: writable or foreign release ancestor: $directory" >&2
        exit 1
    fi
    test "$directory" != / || break
    directory=$(dirname -- "$directory")
done

files=(
    rustos-linux-dvm-x86_64.bzImage
    rustos-linux-dvm-x86_64.rootfs.cpio.xz
    rustos-linux-dvm-x86_64.config
    rustos-linux-dvm-x86_64.kernel.config
    rustos-linux-dvm-x86_64.module-signing.x509
    rustos-linux-dvm-x86_64.sources.lock
    rustos-linux-dvm-x86_64.control.env
    rustos-linux-dvm-x86_64.manifest
)
for file in "${files[@]}"; do
    source="$artifact_dir/$file"
    test -f "$source" && test ! -L "$source" || {
        echo "rustos-linux-dvm: release artifact missing or symlinked: $source" >&2
        exit 1
    }
done
"$(dirname -- "$0")/verify-release-artifacts.sh" "$artifact_dir"

tmp=$(mktemp -d "$parent/.${name}.tmp.XXXXXX")
cleanup() {
    if test -n "${tmp:-}" && test -d "$tmp"; then
        find "$tmp" -maxdepth 1 -type f -delete
        rmdir -- "$tmp"
    fi
}
trap cleanup EXIT

for file in "${files[@]}"; do
    install -m 0644 -- "$artifact_dir/$file" "$tmp/$file"
    sync -f "$tmp/$file"
done

"$(dirname -- "$0")/verify-release-artifacts.sh" "$tmp"

chmod 0755 "$tmp"
sync -f "$tmp"
mv -T -n -- "$tmp" "$destination"
test ! -d "$tmp" || {
    echo "rustos-linux-dvm: destination appeared during atomic publication: $destination" >&2
    exit 1
}
tmp=''
sync -f "$parent"
printf 'rustos-linux-dvm: staged immutable release %s\n' \
    "$destination/rustos-linux-dvm-x86_64.manifest"
