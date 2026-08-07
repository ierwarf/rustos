#!/usr/bin/env bash
# Reclaim regenerable build caches, and refuse to touch anything expensive.
#
# The distinction this script exists to hold is not "generated vs source" -
# almost everything large here is generated. It is "cheap to regenerate vs
# not". `driver-domains/linux/out/buildroot-output` is 26 GiB of generated
# files and deleting it costs a multi-hour DVM toolchain rebuild that also
# fails `verify-dvm` and every KVM run until it finishes. That tree is
# protected here by name, permanently, because a disk-pressure reflex is
# exactly when someone would remove it.
set -euo pipefail

repo_root="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$repo_root"

usage() {
    cat <<'EOF'
Usage: tools/reclaim-build-space.sh [--dry-run] [--aggressive]

  (default)      Formal-lane cargo target caches and captured perf data.
  --aggressive   Also the host cargo target directory. Costs a full host
                 rebuild; refuses to run while a cargo process is active.
  --dry-run      Report sizes and exit without deleting.

Never removed, at any level: driver-domains/linux/out (Buildroot toolchain,
sysroot, and download cache), build/image, build/rustos-boot.img, and every
*.json evidence summary under build/formal.
EOF
}

dry_run=0
aggressive=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        --aggressive) aggressive=1 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$arg" >&2; usage >&2; exit 2 ;;
    esac
done

# Every entry is a cargo target directory or a capture file that the lane
# regenerates on its next run. Evidence summaries live beside them and are
# deliberately absent from this list.
cheap_targets=(
    "build/formal/sanitizers/target-address"
    "build/formal/sanitizers/target-address-buildstd-v1"
    "build/formal/sanitizers/target-thread"
    "build/formal/sanitizers/target-thread-buildstd-v1"
    "build/formal/implementation-mutations/target"
    "build/formal/abi-differential"
    "build/formal/shuttle"
    "build/perf"
    "target/miri"
    "target/kani"
)

size_of() {
    [[ -e "$1" ]] || { printf '0\n'; return; }
    du -sk "$1" 2>/dev/null | cut -f1
}

human() {
    numfmt --to=iec --suffix=B --from-unit=1024 "$1" 2>/dev/null || printf '%sK\n' "$1"
}

reclaimed_kb=0
remove() {
    local path="$1"
    [[ -e "$path" ]] || return 0
    # A lane's target directory is only reclaimable while that lane is idle.
    if [[ "$path" == build/formal/* ]] \
        && pgrep -f 'formal/run-.*\.(py|sh)' >/dev/null 2>&1; then
        printf 'skipping active formal lane cache: %s\n' "$path" >&2
        return 0
    fi
    # Refuse anything under the protected tree even if a future edit adds it.
    case "$path" in
        driver-domains/linux/out*|build/image*|build/rustos-boot.img)
            printf 'refusing protected path: %s\n' "$path" >&2
            return 0
            ;;
    esac
    local kb
    kb="$(size_of "$path")"
    reclaimed_kb=$((reclaimed_kb + kb))
    if [[ "$dry_run" -eq 1 ]]; then
        printf '  would remove %-56s %s\n' "$path" "$(human "$kb")"
    else
        rm -rf -- "$path"
        printf '  removed      %-56s %s\n' "$path" "$(human "$kb")"
    fi
}

printf 'reclaimable formal-lane caches:\n'
for path in "${cheap_targets[@]}"; do
    remove "$path"
done
# Captured profiles are re-recorded by the run that wants them.
while IFS= read -r -d '' capture; do
    remove "${capture#./}"
done < <(find ./build -maxdepth 1 -name 'perf*.data' -print0 2>/dev/null)

if [[ "$aggressive" -eq 1 ]]; then
    if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; then
        printf 'refusing --aggressive: a cargo or rustc process is running\n' >&2
        exit 1
    fi
    printf 'host cargo target directory:\n'
    remove "target/debug"
    remove "target/x86_64-unknown-linux-gnu"
fi

printf 'reclaimed %s\n' "$(human "$reclaimed_kb")"
df -h . | tail -1
