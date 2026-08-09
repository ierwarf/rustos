#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CHECK_AI=0
CHECK_DOCS=0
CHECK_FORMAL=0
CHECK_PHYSICAL=0
CHECK_RELEASE=0
failures=0

usage() {
    cat <<'EOF'
usage: tools/check-dev-environment.sh [OPTIONS]

Read-only RustOS development environment diagnosis. The base check validates
the pinned Rust toolchain and common host tools. Optional modes add their own
requirements without installing packages or changing host configuration.

  --ai            require the project Codex/MCP launchers
  --docs          require the pinned mdBook version
  --formal        require formal host tools and validate the model registry
  --physical-gpu  require QEMU 11+, KVM, IOMMUFD, and VFIO device nodes
  --release       require configured GRUB signing inputs
EOF
}

ok() { printf 'ok - %s\n' "$*"; }
bad() { printf 'not ok - %s\n' "$*" >&2; failures=$((failures + 1)); }

require_command() {
    local command=$1
    if command -v "$command" >/dev/null 2>&1; then
        ok "$command available"
    else
        bad "$command missing"
    fi
}

while (($#)); do
    case "$1" in
        --ai) CHECK_AI=1 ;;
        --docs) CHECK_DOCS=1 ;;
        --formal) CHECK_FORMAL=1 ;;
        --physical-gpu) CHECK_PHYSICAL=1 ;;
        --release) CHECK_RELEASE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; bad "unknown option: $1"; exit 2 ;;
    esac
    shift
done

cd -- "$ROOT"
for command in bash cargo git jq make rustc rustup sha256sum; do
    require_command "$command"
done

expected_toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
active_toolchain=$(rustup show active-toolchain 2>/dev/null | awk '{print $1}' || true)
if test -n "$expected_toolchain" && test "$active_toolchain" = "$expected_toolchain-x86_64-unknown-linux-gnu"; then
    ok "active Rust toolchain matches $expected_toolchain"
else
    bad "active Rust toolchain '$active_toolchain' does not match '$expected_toolchain'"
fi

if jq -s -e '
    .[0] as $launch
    | .[1] as $settings
    | $launch.version == "0.2.0"
      and ($launch.configurations | type) == "array"
      and ($launch.configurations | length) == 1
      and ($launch.configurations[0]
        | .name == "RustOS: verified KVM desktop"
          and .type == "node-terminal"
          and .request == "launch"
          and .command == "exec cargo xtask kvm-run --build --rustos-vcpus 8"
          and .cwd == "${workspaceFolder}")
      and ($launch.configurations[0] | has("preLaunchTask") | not)
      and ([$launch | .. | strings | select(. == "build-dvm")] | length) == 0
      and ($settings | type) == "object"
  ' .vscode/launch.json .vscode/settings.json >/dev/null \
    && test ! -e .vscode/tasks.json; then
    ok "VS Code F5 uses one signed-build plus verified cached-DVM command"
else
    bad "VS Code F5 contract is split, malformed, or rebuilds the Linux DVM"
fi

if ! grep -Eq '^[[:space:]]*rustc-wrapper[[:space:]]*=' .cargo/config.toml; then
    ok "repository Cargo config does not require an optional rustc cache"
else
    bad "repository Cargo config makes an optional rustc cache a mandatory F5 dependency"
fi

if test "$CHECK_AI" -eq 1; then
    require_command npx
    require_command uvx
    grep -q 'mcp-ripgrep@0.4.0' .codex/config.toml || bad "ripgrep MCP pin missing"
    grep -q 'serena-agent==1.6.0' .codex/config.toml || bad "Serena MCP pin missing"
    if .codex/hooks/selftest.sh >/dev/null; then
        ok "Codex hooks, handoff, skill, and Serena contracts are consistent"
    else
        bad "Codex AI infrastructure selftest failed"
    fi
fi

if test "$CHECK_DOCS" -eq 1; then
    require_command mdbook
    mdbook_version=$(mdbook --version 2>/dev/null || true)
    case "$mdbook_version" in
        *'0.4.52'*) ok "mdBook version is 0.4.52" ;;
        *) bad "mdBook version is not 0.4.52: $mdbook_version" ;;
    esac
fi

if test "$CHECK_FORMAL" -eq 1; then
    for command in clang curl pkg-config python3 timeout; do
        require_command "$command"
    done
    require_command java
    java_major=$(java -version 2>&1 | sed -n '1s/.*version "\([0-9]*\).*/\1/p')
    if test -n "$java_major" && test "$java_major" -ge 17; then
        ok "Java major version is $java_major"
    else
        bad "Java 17 or newer is required"
    fi
    if bash formal/selftest.sh >/dev/null; then
        ok "formal model registry is internally consistent"
    else
        bad "formal model registry selftest failed"
    fi
fi

if test "$CHECK_PHYSICAL" -eq 1; then
    require_command qemu-system-x86_64
    qemu_major=$(qemu-system-x86_64 --version 2>/dev/null | sed -n '1s/.*version \([0-9]*\).*/\1/p')
    if test -n "$qemu_major" && test "$qemu_major" -ge 11; then
        ok "QEMU major version is $qemu_major"
    else
        bad "QEMU 11 or newer is required"
    fi
    test -c /dev/kvm && test -r /dev/kvm && test -w /dev/kvm \
        && ok "/dev/kvm is usable" || bad "/dev/kvm is not directly usable"
    test -c /dev/iommu && test -r /dev/iommu && test -w /dev/iommu \
        && ok "/dev/iommu is usable" || bad "/dev/iommu is not directly usable"
    test -d /dev/vfio/devices \
        && ok "/dev/vfio/devices exists" || bad "/dev/vfio/devices missing"
fi

if test "$CHECK_RELEASE" -eq 1; then
    require_command gpg
    test -n "${RUSTOS_GRUB_SIGNING_KEY:-}" \
        && ok "RUSTOS_GRUB_SIGNING_KEY is set" || bad "RUSTOS_GRUB_SIGNING_KEY is unset"
    test -n "${RUSTOS_GPG_HOME:-}" && test -d "${RUSTOS_GPG_HOME:-}" \
        && ok "RUSTOS_GPG_HOME exists" || bad "RUSTOS_GPG_HOME is absent or invalid"
fi

if test "$failures" -ne 0; then
    printf 'check-dev-environment: %d failure(s)\n' "$failures" >&2
    exit 1
fi
printf 'check-dev-environment: passed\n'
