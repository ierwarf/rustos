#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="${ROOT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
GPG_HOME="${RUSTOS_GPG_HOME:-${ROOT_DIR}/build/dev-grub-gpg}"
PUBKEY="${RUSTOS_GRUB_PUBKEY:-${ROOT_DIR}/build/dev-grub.pub}"
SIGNING_KEY="${RUSTOS_GRUB_SIGNING_KEY:-RustOS VSCode Dev <rustos-vscode-dev@example.invalid>}"

mkdir -p "${GPG_HOME}"
chmod 700 "${GPG_HOME}"

if ! gpg --homedir "${GPG_HOME}" --batch --list-secret-keys "${SIGNING_KEY}" >/dev/null 2>&1; then
  gen_log="${GPG_HOME}/quick-gen-key.log"
  if ! gpg \
    --homedir "${GPG_HOME}" \
    --batch \
    --passphrase '' \
    --pinentry-mode loopback \
    --quick-gen-key "${SIGNING_KEY}" rsa2048 sign 0 >"${gen_log}" 2>&1; then
    gpg --homedir "${GPG_HOME}" --batch --list-secret-keys "${SIGNING_KEY}" >/dev/null 2>&1 \
      || gpg --homedir "${GPG_HOME}" --batch --list-keys "${SIGNING_KEY}" >/dev/null \
      || { cat "${gen_log}" >&2; exit 1; }
  fi
fi

mkdir -p "$(dirname "${PUBKEY}")"
gpg --homedir "${GPG_HOME}" --batch --export "${SIGNING_KEY}" > "${PUBKEY}"

echo "GRUB dev signing key ready: ${PUBKEY}"
