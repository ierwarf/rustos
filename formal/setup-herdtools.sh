#!/usr/bin/env bash
# Extract a hash-pinned herdtools7 package below build/formal without install.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
read -r version package_url package_sha source_url source_sha < <(
    python3 - <<'PY'
import tomllib
with open("formal/herdtools.lock", "rb") as handle:
    lock = tomllib.load(handle)
print(lock["version"], lock["package_url"], lock["package_sha256"], lock["source_url"], lock["source_sha256"])
PY
)

missing=()
for tool in curl dpkg-deb; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if ((${#missing[@]})); then
    echo "cannot extract pinned herdtools7 $version; missing host prerequisites: ${missing[*]}" >&2
    echo "The package is extracted below build/formal without privilege escalation; install only the listed host utility and rerun." >&2
    exit 1
fi

tools_dir="$repo_root/build/formal/tools"
install_dir="$tools_dir/herdtools7-$version"
package="$tools_dir/herdtools7-$version-amd64.deb"
if [[ ! -f "$package" ]] || [[ "$(sha256sum "$package" | awk '{print $1}')" != "$package_sha" ]]; then
    mkdir -p "$tools_dir"
    temporary_package="$(mktemp "$tools_dir/herdtools7-$version.download.XXXXXX")"
    trap 'rm -f "$temporary_package"' EXIT
    curl --fail --location "$package_url" -o "$temporary_package"
    actual_sha="$(sha256sum "$temporary_package" | awk '{print $1}')"
    [[ "$actual_sha" == "$package_sha" ]] || {
        echo "herdtools7 package SHA-256 mismatch: got $actual_sha" >&2
        exit 1
    }
    mv "$temporary_package" "$package"
    trap - EXIT
fi
source_archive="$tools_dir/herdtools7-$version-source.tar.gz"
source_tree="$tools_dir/herdtools7-$version-source"
if [[ ! -f "$source_archive" ]] || [[ "$(sha256sum "$source_archive" | awk '{print $1}')" != "$source_sha" ]]; then
    temporary_source="$(mktemp "$tools_dir/herdtools7-$version.source.XXXXXX")"
    trap 'rm -f "$temporary_source"' EXIT
    curl --fail --location --proto '=https' --tlsv1.2 "$source_url" -o "$temporary_source"
    actual_source_sha="$(sha256sum "$temporary_source" | awk '{print $1}')"
    [[ "$actual_source_sha" == "$source_sha" ]] || {
        echo "herdtools7 source SHA-256 mismatch: got $actual_source_sha" >&2
        exit 1
    }
    mv "$temporary_source" "$source_archive"
    trap - EXIT
fi
if [[ ! -f "$source_tree/herd/libdir/x86tso.cat" ]]; then
    temporary_source_tree="$(mktemp -d "$tools_dir/herdtools7-$version.source-extract.XXXXXX")"
    trap 'rm -rf "$temporary_source_tree"' EXIT
    tar -xzf "$source_archive" -C "$temporary_source_tree" --strip-components=1
    rm -rf "$source_tree"
    mv "$temporary_source_tree" "$source_tree"
    trap - EXIT
fi
if [[ ! -x "$install_dir/usr/bin/herd7" ]]; then
    temporary_dir="$(mktemp -d "$tools_dir/herdtools7-$version.extract.XXXXXX")"
    trap 'rm -rf "$temporary_dir"' EXIT
    dpkg-deb -x "$package" "$temporary_dir"
    rm -rf "$install_dir"
    mv "$temporary_dir" "$install_dir"
    trap - EXIT
fi
"$install_dir/usr/bin/herd7" -version
printf 'Pinned herd7 is ready at %s\n' "$install_dir/usr/bin/herd7"
printf 'Pinned herd7 cat models are ready at %s\n' "$source_tree/herd/libdir"
