#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail
IFS=$'\n\t'

SERENA_VERSION=1.6.0
AST_GREP_VERSION=0.45.2
AST_GREP_MCP_COMMIT=149e20d47bb7125fb0c1451feea2f48a98742034
CODEGRAPH_VERSION=0.20.1
RIPGREP_MCP_VERSION=0.4.0

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
    local urls=()
    mapfile -t urls < <(pacman -Sp --print-format '%l' "$@")
    ((${#urls[@]} > 0)) || { echo "no Arch package URLs for: $*" >&2; return 1; }
    local url archive
    for url in "${urls[@]}"; do
        archive="$CACHE_ROOT/$(basename "$url")"
        [[ -s "$archive" ]] || fetch_url "$url" "$archive"
        bsdtar -xf "$archive" -C "$root"
    done
}

ensure_uv() {
    if command -v uv >/dev/null 2>&1 && command -v uvx >/dev/null 2>&1; then
        log "uv=$(command -v uv)"
        return
    fi
    local root="$TOOLS_ROOT/arch-uv"
    rm -rf -- "$root"
    mkdir -p "$root"
    extract_arch_packages "$root" uv
    install -m 0755 "$root/usr/bin/uv" "$BIN_DIR/uv"
    if [[ -e "$root/usr/bin/uvx" ]]; then
        install -m 0755 "$root/usr/bin/uvx" "$BIN_DIR/uvx"
    else
        ln -sfn uv "$BIN_DIR/uvx"
    fi
}

ensure_node_stack() {
    if command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1; then
        log "node=$(command -v node) npm=$(command -v npm) npx=$(command -v npx)"
        return
    fi

    local root="$TOOLS_ROOT/arch-node"
    rm -rf -- "$root"
    mkdir -p "$root"
    extract_arch_packages "$root" nodejs npm

    local node_bin="$root/usr/bin/node"
    local npm_cli="$root/usr/lib/node_modules/npm/bin/npm-cli.js"
    local npx_cli="$root/usr/lib/node_modules/npm/bin/npx-cli.js"
    [[ -x "$node_bin" && -f "$npm_cli" && -f "$npx_cli" ]] || {
        echo "incomplete Node/npm payload under $root" >&2
        return 1
    }

    cat >"$BIN_DIR/node" <<EOF
#!/usr/bin/env bash
export LD_LIBRARY_PATH="$root/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$node_bin" "\$@"
EOF
    cat >"$BIN_DIR/npm" <<EOF
#!/usr/bin/env bash
export LD_LIBRARY_PATH="$root/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$node_bin" "$npm_cli" "\$@"
EOF
    cat >"$BIN_DIR/npx" <<EOF
#!/usr/bin/env bash
export LD_LIBRARY_PATH="$root/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$node_bin" "$npx_cli" "\$@"
EOF
    chmod 0755 "$BIN_DIR/node" "$BIN_DIR/npm" "$BIN_DIR/npx"
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

install_uv_tools() {
    uv tool install --force -p 3.13 "serena-agent==$SERENA_VERSION"
    uv tool install --force -p 3.13 "ast-grep-cli==$AST_GREP_VERSION"
    uv tool install --force -p 3.13 \
        "git+https://github.com/ast-grep/ast-grep-mcp.git@$AST_GREP_MCP_COMMIT"

    link_uv_tool serena
    link_uv_tool ast-grep
    link_uv_tool ast-grep-server

    if [[ ! -f "$HOME/.serena/serena_config.yml" ]]; then
        serena init -b LSP
    fi
}

install_npm_tools() {
    local prefix="$TOOLS_ROOT/npm-global"
    local codegraph_root="$prefix/lib/node_modules/@astudioplus/codegraph-mcp"
    mkdir -p "$prefix"
    export npm_config_prefix="$prefix"
    export npm_config_cache="$HOME/.cache/npm"
    export CODEGRAPH_SKIP_MODEL_FETCH=1

    # This function is entered only when the persisted stack failed validation.
    # Remove the CodeGraph package so a prior lifecycle-script-denied partial
    # install cannot survive as an npm "up to date" false positive.
    rm -rf -- "$codegraph_root"
    rm -f -- "$prefix/bin/codegraph-mcp" "$BIN_DIR/codegraph-mcp"
    npm install --global --no-audit --no-fund \
        --allow-scripts=@astudioplus/codegraph-mcp \
        "@astudioplus/codegraph-mcp@$CODEGRAPH_VERSION" \
        "mcp-ripgrep@$RIPGREP_MCP_VERSION"

    local name
    for name in codegraph-mcp mcp-ripgrep; do
        [[ -e "$prefix/bin/$name" ]] || { echo "npm package did not expose $name" >&2; return 1; }
        ln -sfn "$prefix/bin/$name" "$BIN_DIR/$name"
    done
}

install_project_rust_analyzer() {
    local root=${1:?repository root required} channel
    channel="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")"
    [[ -n "$channel" ]] || { echo 'unable to read pinned Rust channel' >&2; return 1; }
    rustup toolchain install "$channel" --profile minimal --component rust-src --component rust-analyzer
}

write_manifest() {
    cat >"$MANIFEST" <<EOF
SERENA_VERSION=$SERENA_VERSION
AST_GREP_VERSION=$AST_GREP_VERSION
AST_GREP_MCP_COMMIT=$AST_GREP_MCP_COMMIT
CODEGRAPH_VERSION=$CODEGRAPH_VERSION
RIPGREP_MCP_VERSION=$RIPGREP_MCP_VERSION
EOF
}

pinned_stack_is_current() {
    local prefix="$TOOLS_ROOT/npm-global"
    local codegraph_root="$prefix/lib/node_modules/@astudioplus/codegraph-mcp"
    local ripgrep_root="$prefix/lib/node_modules/mcp-ripgrep"
    local native_codegraph="$codegraph_root/bin/codegraph-server-linux-x64"

    [[ -s "$MANIFEST" ]] || return 1
    grep -qx "SERENA_VERSION=$SERENA_VERSION" "$MANIFEST" || return 1
    grep -qx "AST_GREP_VERSION=$AST_GREP_VERSION" "$MANIFEST" || return 1
    grep -qx "AST_GREP_MCP_COMMIT=$AST_GREP_MCP_COMMIT" "$MANIFEST" || return 1
    grep -qx "CODEGRAPH_VERSION=$CODEGRAPH_VERSION" "$MANIFEST" || return 1
    grep -qx "RIPGREP_MCP_VERSION=$RIPGREP_MCP_VERSION" "$MANIFEST" || return 1

    command -v serena >/dev/null 2>&1 || return 1
    command -v ast-grep >/dev/null 2>&1 || return 1
    command -v ast-grep-server >/dev/null 2>&1 || return 1
    command -v codegraph-mcp >/dev/null 2>&1 || return 1
    command -v mcp-ripgrep >/dev/null 2>&1 || return 1
    [[ -f "$HOME/.serena/serena_config.yml" ]] || return 1
    [[ -x "$native_codegraph" ]] || return 1

    serena --version 2>/dev/null | grep -q "${SERENA_VERSION//./\\.}" || return 1
    ast-grep --version 2>/dev/null | grep -q "${AST_GREP_VERSION//./\\.}" || return 1
    [[ "$(node -p "require('$codegraph_root/package.json').version" 2>/dev/null)" == "$CODEGRAPH_VERSION" ]] || return 1
    [[ "$(node -p "require('$ripgrep_root/package.json').version" 2>/dev/null)" == "$RIPGREP_MCP_VERSION" ]] || return 1
    ast-grep-server --help >/dev/null 2>&1 || return 1
    codegraph-mcp --help >/dev/null 2>&1 || return 1
}

main() {
    local root=${1:-$(git rev-parse --show-toplevel)}
    root="$(cd -- "$root" && pwd -P)"

    ensure_uv
    ensure_node_stack
    ensure_clangd
    install_project_rust_analyzer "$root"

    if pinned_stack_is_current; then
        log 'pinned AI tool stack already valid; skipping reinstall'
    else
        install_uv_tools
        install_npm_tools
        write_manifest
        pinned_stack_is_current || {
            echo 'pinned AI tool stack failed post-install validation' >&2
            return 1
        }
    fi

    log 'installed versions'
    serena --version
    ast-grep --version
    node --version
    npm --version
    clangd --version | sed -n '1p'
    cat "$MANIFEST"

    log "persistent bin=$BIN_DIR"
    log "persistent tools=$TOOLS_ROOT"
}

main "$@"
