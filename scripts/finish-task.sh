#!/usr/bin/env bash
# Called by run-task OUTSIDE the agent sandbox. Never merges or publishes.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
message="${1:-}"
pr_title="${2:-$message}"
[[ -n "$message" ]] || { echo 'Usage: finish-task.sh "type: commit message" ["PR title"]' >&2; exit 2; }
branch="$(git branch --show-current)"
if [[ ! "$branch" =~ ^(feature|fix|refactor|docs|test|chore|security)/[a-z0-9][a-z0-9._-]*$ ]] || [[ "$branch" == *codex* ]]; then
  echo 'Invalid task branch; refusing to commit or push.' >&2; exit 1
fi
origin_url="$(git remote get-url origin 2>/dev/null)" || { echo 'No origin configured.' >&2; exit 1; }
case "$origin_url" in
  https://github.com/thrns/thrn|https://github.com/thrns/thrn.git|git@github.com:thrns/thrn.git|ssh://git@github.com:thrns/thrn.git) ;;
  *) echo "Unexpected origin for THRN: $origin_url" >&2; exit 1 ;;
esac
git config --local core.hooksPath .githooks
command -v gh >/dev/null && gh auth status >/dev/null 2>&1 || { echo 'GitHub CLI authentication required.' >&2; exit 1; }
mkdir -p .thrn-reports
logfile=".thrn-reports/${branch//\//-}-local.log"
printf 'Running local verification; log: %s\n' "$logfile"
# tee and pipefail preserve failing verifier exit status.
bash scripts/verify.sh 2>&1 | tee "$logfile"
# Intentionally stage changes only after checks. Inspect all paths before a commit.
git add -A
git diff --cached --check
if git diff --cached --quiet; then echo 'No changes to commit; refusing a no-op delivery.' >&2; exit 1; fi
while IFS= read -r path; do
  lower="$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')"
  if [[ "$lower" == ".env.example" || "$lower" == */.env.example ]]; then continue; fi
  if [[ "$lower" =~ (^|/)(\.env($|\.)|id_rsa|id_ed25519|auth\.json|credentials\.json|\.npmrc|\.pypirc) ]] || [[ "$lower" =~ \.(pem|p12|pfx|key)$ ]]; then
    echo "Sensitive-looking staged file: $path. Refusing push." >&2; exit 1
  fi
done < <(git diff --cached --name-only)
# Both human-readable review evidence and machine comparison source.
git diff --cached --stat
printf '\nStaged files:\n'
git diff --cached --name-only
printf '\n==> commit\n'
git commit -m "$message"
sha="$(git rev-parse HEAD)"
# Hook re-runs checks to catch pre-commit differences.
printf '\n==> push feature branch\n'
git push -u origin "$branch"
printf '\n==> create or locate pull request\n'
if gh pr view "$branch" --json url --jq '.url' >/dev/null 2>&1; then
  pr_url="$(gh pr view "$branch" --json url --jq '.url')"
else
  pr_url="$(gh pr create --draft --base main --head "$branch" --title "$pr_title" --body-file .github/pull_request_template.md)"
fi
printf '\n=== DELIVERY REPORT ===\n'
printf 'branch: %s\ncommit: %s\npull_request: %s\nlocal_checks: PASS\nlocal_log: %s\n' "$branch" "$sha" "$pr_url" "$logfile"
printf 'changed_files:\n'
git diff-tree --no-commit-id --name-only -r "$sha"
printf '\nRemote checks are pending until independently confirmed.\n'
printf 'Review using: gh pr diff "%s" and gh pr checks "%s" --watch\n' "$pr_url" "$pr_url"
