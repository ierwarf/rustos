#!/usr/bin/env bash
# Shared PostToolUse hook for Edit/Write, wired from both .codex/config.toml
# and .claude/settings.json.
#
# After Rust source edits, run a fast type check plus the source-contract
# header linter so the agent loop gets immediate feedback instead of
# discovering the break, or a missing //! contract field, much later at the
# formal PR gate. Only runs inside the RustOS workspace; no-op elsewhere.

set -euo pipefail

INPUT="$(cat)"

path="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.file_path // .tool_input.relative_path //
  .arguments.file_path // .arguments.relative_path //
  .params.file_path // .params.relative_path // empty
' 2>/dev/null || true)"
cmd="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .arguments.command // .params.command // empty
' 2>/dev/null || true)"

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
case "$path" in
  /*) abs_path="$path" ;;
  "") abs_path="" ;;
  *) abs_path="$REPO_ROOT/$path" ;;
esac

# Only react to *.rs edits inside this workspace. apply_patch reports the patch
# as tool_input.command, so parse file headers when no direct file path exists.
case "$abs_path" in
  "$REPO_ROOT"/*.rs|"$REPO_ROOT"/**/*.rs) ;;
  *)
    if ! printf '%s\n' "$cmd" | grep -Eq '^\*\*\* (Add|Update|Delete) File: .+\.rs$'; then
      exit 0
    fi
    ;;
esac

cd "$REPO_ROOT"

# Cache only an identical worktree state. A time-only global stamp can
# incorrectly skip a second edit, another clone, or another worktree. Hash the
# tracked, staged, and untracked content and namespace the stamp by the
# canonical repository path instead.
repo_key="$(printf '%s' "$REPO_ROOT" | sha256sum | awk '{print $1}')"
STAMP="${TMPDIR:-/tmp}/rustos-post-edit-ok-${repo_key}"
workspace_fingerprint="$({
  git diff --no-ext-diff --binary
  git diff --cached --no-ext-diff --binary
  while IFS= read -r -d '' file; do
    printf 'untracked:%q:' "$file"
    if [[ -L "$file" ]]; then
      printf 'symlink:%s\n' "$(readlink -- "$file")"
    elif [[ -f "$file" ]]; then
      sha256sum -- "$file"
    else
      stat --printf='special:%F:%s:%f\n' -- "$file"
    fi
  done < <(git ls-files --others --exclude-standard -z)
} | sha256sum | awk '{print $1}')"

if [[ -f "$STAMP" ]] && [[ "$(cat "$STAMP" 2>/dev/null || true)" == "$workspace_fingerprint" ]]; then
  exit 0
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

if ! timeout 90 cargo xtask check >"$log" 2>&1; then
  tail="$(tail -n 40 "$log")"
  jq -n --arg m "cargo xtask check failed (tail):
$tail" '{systemMessage:$m}'
  exit 0
fi

# Cheap (well under a second for the whole tree): catches a missing //!
# contract header field, an undocumented critical/high boundary, or a stale
# retired-path reference immediately, instead of only at the formal PR gate.
if ! timeout 15 python3 formal/check-rust-source-contracts.py >"$log" 2>&1; then
  tail="$(tail -n 40 "$log")"
  jq -n --arg m "formal/check-rust-source-contracts.py failed (tail):
$tail" '{systemMessage:$m}'
  exit 0
fi

printf '%s\n' "$workspace_fingerprint" >"$STAMP"
