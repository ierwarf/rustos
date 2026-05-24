#!/usr/bin/env bash
# Codex UserPromptSubmit hook.
# Blocks obvious secret material before it is added to the model-visible prompt.

set -euo pipefail

INPUT="$(cat)"
prompt="$(printf '%s' "$INPUT" | jq -r '.prompt // empty' 2>/dev/null || true)"

[[ -z "$prompt" ]] && exit 0

block() {
  local reason="$1"
  jq -n --arg m "$reason" '{decision:"block", reason:$m}'
  exit 0
}

if printf '%s' "$prompt" | grep -Eq -- '-----BEGIN (RSA |DSA |EC |OPENSSH |PGP )?PRIVATE KEY-----'; then
  block "Prompt appears to contain a private key. Remove the secret and retry."
fi

if printf '%s' "$prompt" | grep -Eq '\b(sk-[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{20,})\b'; then
  block "Prompt appears to contain an API or GitHub/Slack token. Remove the secret and retry."
fi

if printf '%s' "$prompt" | grep -Eq '\bAKIA[0-9A-Z]{16}\b'; then
  block "Prompt appears to contain an AWS access key id. Remove the secret and retry."
fi

exit 0
