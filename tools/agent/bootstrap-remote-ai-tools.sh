#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail
IFS=$'\n\t'

SERENA_VERSION=1.6.0
BIN_DIR="$HOME/.local/bin"
TOOLS_ROOT="$HOME/.local/share/rustos-agent-tools"
CACHE_ROOT="$HOME/.cache/rustos-agent-tools"
MANIFEST="$TOOLS_ROOT/manifest.env"
mkdir -p "$BIN_DIR" "$TOOLS_ROOT" "$CACHE_ROOT"
export PATH="$BIN_DIR:$PATH"

log() { printf 'agent-tools: %s\n' "$*"; }

fetch_url() {
    local url=$1 output=$2
    case "$url" in
        file://*) cp -- "${url#file://}" "$output" ;;
        *) curl --fail --location --retry 3 --silent --show-error "$url" -o "$output" ;;
    esac
}

extract_arch_packages() {
    local root=$1
    shift
    command -v pacman >/dev/null 2>&1 || { echo 'pacman is required' >&2; return 1; }
    command -v bsdtar >/dev/null 2>&1 || { echo 'bsdtar is required' >&2; return 1; }
    mkdir -p "$root"
    local urls=() url archive
    mapfile -t urls < <(pacman -Sp --print-format '%l' "$@")
    ((${#urls[@]} > 0)) || { echo "no Arch package URLs for: $*" >&2; return 1; }
    for url in "${urls[@]}"; do
        archive="$CACHE_ROOT/$(basename "$url")"
        [[ -s "$archive" ]] || fetch_url "$url" "$archive"
        bsdtar -xf "$archive" -C "$root"
    done
}

ensure_uv() {
    if command -v uv >/dev/null 2>&1; then
        log "uv=$(command -v uv)"
        return
    fi
    local root="$TOOLS_ROOT/arch-uv"
    rm -rf -- "$root"
    mkdir -p "$root"
    extract_arch_packages "$root" uv
    install -m 0755 "$root/usr/bin/uv" "$BIN_DIR/uv"
}

ensure_clangd() {
    if command -v clangd >/dev/null 2>&1; then
        log "clangd=$(command -v clangd)"
        return
    fi
    local root="$TOOLS_ROOT/arch-clang"
    rm -rf -- "$root"
    mkdir -p "$root"
    extract_arch_packages "$root" clang
    [[ -x "$root/usr/bin/clangd" ]] || { echo 'clang package did not provide clangd' >&2; return 1; }
    cat >"$BIN_DIR/clangd" <<EOF
#!/usr/bin/env bash
export LD_LIBRARY_PATH="$root/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$root/usr/bin/clangd" "\$@"
EOF
    chmod 0755 "$BIN_DIR/clangd"
}

link_uv_tool() {
    local name=$1 uv_bin_dir source
    uv_bin_dir="$(uv tool dir --bin)"
    source="$uv_bin_dir/$name"
    [[ -e "$source" ]] || { echo "uv tool did not expose $name" >&2; return 1; }
    [[ "$source" == "$BIN_DIR/$name" ]] || ln -sfn "$source" "$BIN_DIR/$name"
}

install_serena() {
    uv tool install --force -p 3.13 "serena-agent==$SERENA_VERSION"
    link_uv_tool serena
    if [[ ! -f "$HOME/.serena/serena_config.yml" ]]; then
        serena init -b LSP
    fi
}

install_project_rust_analyzer() {
    local root=${1:?repository root required} channel
    channel="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")"
    [[ -n "$channel" ]] || { echo 'unable to read pinned Rust channel' >&2; return 1; }
    rustup toolchain install "$channel" --profile minimal --component rust-src --component rust-analyzer
}

write_manifest() {
    printf 'SERENA_VERSION=%s\n' "$SERENA_VERSION" >"$MANIFEST"
}

serena_is_current() {
    [[ -s "$MANIFEST" ]] || return 1
    [[ "$(wc -l < "$MANIFEST")" -eq 1 ]] || return 1
    grep -qx "SERENA_VERSION=$SERENA_VERSION" "$MANIFEST" || return 1
    command -v serena >/dev/null 2>&1 || return 1
    [[ -f "$HOME/.serena/serena_config.yml" ]] || return 1
    serena --version 2>/dev/null | grep -q "${SERENA_VERSION//./\\.}"
}

main() {
    local root=${1:-$(git rev-parse --show-toplevel)}
    root="$(cd -- "$root" && pwd -P)"

    ensure_uv
    ensure_clangd
    install_project_rust_analyzer "$root"

    if serena_is_current; then
        log 'pinned Serena stack already valid; skipping reinstall'
    else
        install_serena
        write_manifest
        serena_is_current || {
            echo 'Serena stack failed post-install validation' >&2
            return 1
        }
    fi

    log 'installed versions'
    serena --version
    clangd --version | sed -n '1p'
    cat "$MANIFEST"
    log "persistent bin=$BIN_DIR"
    log "persistent tools=$TOOLS_ROOT"
}

main "$@"
