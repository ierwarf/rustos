#!/usr/bin/env bash
# One git worktree per agent, so concurrent agents stop invalidating each other's
# seal and overwriting each other's edits in a single checkout.
#
#   tools/agent/agent-worktree.sh new NAME [REF]   create ../rustos-agent-NAME on branch agent/NAME
#   tools/agent/agent-worktree.sh list             show agent worktrees and their state
#   tools/agent/agent-worktree.sh status NAME      commits ahead and uncommitted files
#   tools/agent/agent-worktree.sh remove NAME      remove a clean worktree (never the branch)
#
# It never merges, pushes, or deletes a branch: integrating an agent's slice is a
# decision for the operator, made at a slice boundary after `formal-preflight.sh` and a
# sealed run pass in that worktree.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
parent="$(dirname "$repo_root")"
base="$(basename "$repo_root")"

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
valid_name() { [[ "$1" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "name must match [a-z0-9-]+" >&2; exit 2; }; }
path_of() { printf '%s/%s-agent-%s' "$parent" "$base" "$1"; }

case "${1:-}" in
  new)
    [[ $# -ge 2 && $# -le 3 ]] || usage
    valid_name "$2"
    target="$(path_of "$2")"
    ref="${3:-HEAD}"
    [[ ! -e "$target" ]] || { echo "$target already exists" >&2; exit 1; }
    git worktree add -b "agent/$2" "$target" "$ref"
    cat <<EOF

Created $target on branch agent/$2 (from $ref).
- It has its own build/ and target/: the first build is a full compile and the first
  sealed run recomputes its caches. Seals are per worktree, so an agent here cannot
  invalidate yours, and you cannot invalidate its.
- Work and commit there. Integrate with \`git merge --no-ff agent/$2\` in the main
  checkout when the slice is done, preflight is green and the worktree is sealed.
- Uncommitted work in the main checkout is NOT visible here; commit or cherry-pick
  what the agent needs as its base.
EOF
    ;;
  list)
    git worktree list --porcelain | awk '/^worktree /{p=$2} /^branch refs\/heads\/agent\//{print p, $2}' |
      while read -r path branch; do
        dirty="$(git -C "$path" status --short | wc -l)"
        ahead="$(git rev-list --count "HEAD..${branch#refs/heads/}" 2>/dev/null || echo '?')"
        printf '%-48s %-28s ahead=%s dirty=%s\n' "$path" "${branch#refs/heads/}" "$ahead" "$dirty"
      done
    ;;
  status)
    [[ $# -eq 2 ]] || usage
    valid_name "$2"
    target="$(path_of "$2")"
    [[ -d "$target" ]] || { echo "no worktree at $target" >&2; exit 1; }
    git -C "$target" status --short --branch
    git log --oneline "HEAD..agent/$2" | head -20
    ;;
  remove)
    [[ $# -eq 2 ]] || usage
    valid_name "$2"
    target="$(path_of "$2")"
    [[ -d "$target" ]] || { echo "no worktree at $target" >&2; exit 1; }
    if [[ -n "$(git -C "$target" status --short)" ]]; then
      echo "refusing to remove $target: it has uncommitted changes" >&2
      exit 1
    fi
    git worktree remove "$target"
    echo "removed $target (branch agent/$2 kept)"
    ;;
  *) usage ;;
esac
