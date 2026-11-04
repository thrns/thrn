#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
branch="${1:-}"
if [[ ! "$branch" =~ ^(feature|fix|refactor|docs|test|chore|security)/[a-z0-9][a-z0-9._-]*$ ]] || [[ "$branch" == *codex* ]]; then
  echo 'Branch must be e.g. feature/m0-foundation; no prohibited agent name.' >&2; exit 2
fi
[[ -z "$(git status --porcelain)" ]] || { echo 'Working tree not clean; resolve or commit existing changes.' >&2; exit 1; }
git remote get-url origin >/dev/null || { echo 'No origin configured.' >&2; exit 1; }
git fetch origin main
git switch main
git pull --ff-only origin main
git show-ref --verify --quiet "refs/heads/$branch" && { echo "Local branch already exists: $branch" >&2; exit 1; }
git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1 && { echo "Remote branch already exists: $branch" >&2; exit 1; }
git switch -c "$branch"
printf 'Created branch: %s\n' "$branch"
