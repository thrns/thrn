#!/usr/bin/env bash
# One-time local initialization. Does NOT create/push a remote repository.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ ! -e .git ]] || { echo 'Git repository already exists; refusing to reinitialize.' >&2; exit 1; }
command -v git >/dev/null || { echo 'git not found' >&2; exit 1; }
git config --global user.name >/dev/null || { echo 'Set git config --global user.name first.' >&2; exit 1; }
git config --global user.email >/dev/null || { echo 'Set git config --global user.email first.' >&2; exit 1; }
git init -b main
git config core.hooksPath .githooks
bash scripts/verify.sh
git add -A
if git diff --cached --name-only | grep -vE '(^|/)\.env\.example$' | grep -E '(^|/)(\.env($|\.)|id_rsa|id_ed25519|auth\.json|credentials\.json)|\.(pem|p12|pfx|key)$'; then
  echo 'Sensitive-looking path staged. Remove it before first commit.' >&2; exit 1
fi
git commit -m 'chore: initialize THRN development workflow'
echo 'Local main initialized. Now create GitHub remote with gh repo create --source=. --remote=origin --push.'
