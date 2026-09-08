#!/usr/bin/env bash
# Compatibility entrypoint. Commit policy now lives in the unified shell hook so
# normal Bash calls pay for only one PreToolUse process.

set -euo pipefail
exec "$(cd "$(dirname "$0")" && pwd -P)/pre_bash_destructive.sh"
