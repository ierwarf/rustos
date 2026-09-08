#!/usr/bin/env bash
# Guard the repository's low-context AI contract against accidental infra drift.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$ROOT"
failures=0

ok() { printf 'ok - %s\n' "$*"; }
bad() { printf 'not ok - %s\n' "$*" >&2; failures=$((failures + 1)); }

max_bytes() {
  local path="$1" limit="$2" size
  size="$(wc -c < "$path")"
  if (( size <= limit )); then
    ok "$path <= ${limit} bytes (${size})"
  else
    bad "$path grew to ${size} bytes; budget is ${limit}"
  fi
}

require_literal() {
  local path="$1" text="$2" label="$3"
  if grep -Fq -- "$text" "$path"; then
    ok "$label"
  else
    bad "$label"
  fi
}

forbid_regex() {
  local path="$1" pattern="$2" label="$3"
  if grep -Eq -- "$pattern" "$path"; then
    bad "$label"
  else
    ok "$label"
  fi
}

read_default() {
  local path="$1" name="$2"
  grep -Eo "${name}:-[0-9]+" "$path" | head -n 1 | sed 's/.*:-//'
}

assert_max_default() {
  local path="$1" name="$2" limit="$3" value
  value="$(read_default "$path" "$name" || true)"
  if [[ "$value" =~ ^[0-9]+$ ]] && (( value <= limit )); then
    ok "$name default <= $limit ($value)"
  else
    bad "$name default missing or above $limit (${value:-missing})"
  fi
}

# Stable/model-visible context budgets. These are ceilings, not targets.
max_bytes AGENTS.md 7000
max_bytes docs/ai/token-policy.md 7000
max_bytes docs/ai-map.md 6000
max_bytes .codex/hooks/session_context.sh 768

initial_prompt_line="$(grep -E '^initial_prompt:' .serena/project.yml || true)"
initial_prompt_bytes="$(printf '%s' "$initial_prompt_line" | wc -c)"
if [[ -n "$initial_prompt_line" ]] && (( initial_prompt_bytes <= 512 )); then
  ok "Serena initial prompt <= 512 bytes (${initial_prompt_bytes})"
else
  bad "Serena initial prompt missing or above 512 bytes (${initial_prompt_bytes})"
fi

require_literal AGENTS.md 'This `AGENTS.md` is the only stable repository' \
  "AGENTS.md remains the sole stable prefix"
require_literal docs/ai/token-policy.md 'reusable repository prefix is exactly `AGENTS.md`' \
  "token policy keeps a one-file stable prefix"
forbid_regex .codex/hooks/session_context.sh 'task-router|token-policy|ai-map|ALL_TOOLS|mcp__' \
  "SessionStart does not inject on-demand policy/tool catalogs"
forbid_regex .serena/project.yml 'initial_prompt:.*(task-router|token-policy|ai-map|ALL_TOOLS|mcp__)' \
  "Serena startup does not preload on-demand policy/tool catalogs"

# Model/tool configuration may get cheaper, but must not silently get heavier.
require_literal .codex/config.toml 'model_verbosity = "low"' \
  "Codex verbosity stays low"
require_literal .codex/config.toml 'model_auto_compact_token_limit_scope = "total"' \
  "Codex compaction scope stays total"
compact_limit="$(sed -n 's/^model_auto_compact_token_limit = \([0-9][0-9]*\)$/\1/p' .codex/config.toml)"
if [[ "$compact_limit" =~ ^[0-9]+$ ]] && (( compact_limit <= 120000 )); then
  ok "auto-compaction threshold <= 120000 (${compact_limit})"
else
  bad "auto-compaction threshold missing or above 120000 (${compact_limit:-missing})"
fi

actual_mcp="$(sed -n 's/^\[mcp_servers\.\([^]]*\)\]$/\1/p' .codex/config.toml | sort)"
expected_mcp=$'ast_grep\ncodegraph\nserena'
if [[ "$actual_mcp" == "$expected_mcp" ]]; then
  ok "MCP surface remains the three non-redundant servers"
else
  bad "MCP surface drifted; expected ast_grep/codegraph/serena only"
fi
forbid_regex .codex/config.toml 'mcp-ripgrep|mcp_servers\.ripgrep' \
  "redundant ripgrep MCP stays disabled"

# Read/output ceilings are allowed to tighten, never to loosen accidentally.
assert_max_default .codex/hooks/pre_read_large_file.sh RUSTOS_HOOK_MAX_READ_LINES 120
assert_max_default .codex/hooks/pre_read_large_file.sh RUSTOS_HOOK_MAX_MCP_ANSWER_CHARS 8000
assert_max_default .codex/hooks/pre_read_large_file.sh RUSTOS_HOOK_MAX_AST_RESULTS 24
assert_max_default .codex/hooks/pre_read_large_file.sh RUSTOS_HOOK_MAX_CODEGRAPH_RESULTS 16
assert_max_default .codex/hooks/pre_bash_destructive.sh RUSTOS_HOOK_MAX_SHELL_OUTPUT_TOKENS 3000
require_literal .agents/hooks/lib.sh 'tail -n 8' "failure tail remains <= 8 lines"
require_literal .agents/hooks/lib.sh 'head -c 2048' "failure payload remains <= 2048 bytes"

# Stale policies that previously caused context/tool inflation must not reappear.
stale_paths=(
  AGENTS.md
  .serena
  .agents/skills
  docs/ai
  docs/ai-map.md
  .codex/config.toml
  tools/agent/bootstrap-remote-ai-tools.sh
  tools/check-dev-environment.sh
)
stale_log="$(mktemp)"
trap 'rm -f "$stale_log"' EXIT
if git grep -n -E 'mcp-ripgrep|mcp_servers\.ripgrep|ripgrep MCP|GPT-5\.4 mini' -- \
    "${stale_paths[@]}" ':(exclude)tools/agent/check-ai-context-contract.sh' >"$stale_log" 2>/dev/null; then
  bad "stale token-heavy AI policy reappeared"
  head -n 8 "$stale_log" >&2
else
  ok "no stale ripgrep-MCP or old sub-agent policy"
fi

# Enforcement chain: local AI commits, remote-agent commits, and CI all run this guard.
require_literal .codex/hooks/selftest.sh 'tools/agent/check-ai-context-contract.sh' \
  "hook selftest invokes context contract"
require_literal .codex/hooks/pre_bash_destructive.sh '.serena/' \
  "commit gate covers Serena AI infrastructure"
require_literal .codex/hooks/pre_bash_destructive.sh 'docs/ai-map\.md' \
  "commit gate covers the root AI map"
require_literal .github/workflows/remote-agent.yml 'tools/agent/check-ai-context-contract.sh' \
  "remote agent validates context contract before commit"
require_literal .github/workflows/ai-context-contract.yml 'tools/agent/check-ai-context-contract.sh' \
  "CI validates context contract"

if (( failures != 0 )); then
  printf 'ai-context-contract: %d failure(s) \n' "$failures" >&2
  exit 1
fi
printf 'ai-context-contract: passed\n'