#!/usr/bin/env bash
# Called by run-task OUTSIDE the agent sandbox. Never merges or publishes.
set -euo pipefail
cd "$(dirname "$BASH_SOURCE")/.."
message=''
pr_title=''
if (($# >= 1)); then message="$1"; fi
if (($# >= 2)); then pr_title="$2"; else pr_title="$message"; fi
[[ -n "$message" ]] || { echo 'Usage: finish-task.sh "type: commit message" ["PR title"]' >&2; exit 2; }
branch="$(git branch --show-current)"
if [[ "$branch" == "main" ]]; then
  echo 'Direct pushes to main are forbidden; use a task branch and pull request.' >&2
  exit 1
fi
if [[ ! "$branch" =~ ^(feature|fix|refactor|docs|test|chore|security)/[a-z0-9][a-z0-9._-]*$ ]] || [[ "$branch" == *codex* ]]; then
  echo 'Invalid task branch; refusing to commit or push.' >&2; exit 1
fi
origin_url="$(git remote get-url origin 2>/dev/null)" || { echo 'No origin configured.' >&2; exit 1; }
case "$origin_url" in
  https://github.com/thrns/thrn|https://github.com/thrns/thrn.git|git@github.com:thrns/thrn.git|ssh://git@github.com/thrns/thrn.git) ;;
  *) echo "Unexpected origin for THRN: $origin_url" >&2; exit 1 ;;
esac
git config --local core.hooksPath .githooks
command -v gh >/dev/null && gh auth status >/dev/null 2>&1 || { echo 'GitHub CLI authentication required.' >&2; exit 1; }
mkdir -p .thrn-reports
branch_slug="$(printf '%s' "$branch" | tr '/' '-')"
logfile=".thrn-reports/$branch_slug-local.log"
printf 'Running local verification; log: %s\n' "$logfile"
# Verification always runs before committing or retrying delivery. The pre-push hook
# runs it again after commit, so a retry cannot bless code that changed after checks.
if ! bash scripts/verify.sh 2>&1 | tee "$logfile"; then
  echo 'Verification failed; refusing to commit, push, or create a pull request.' >&2
  exit 1
fi

# Stage only after successful verification and preserve the existing secret-path gate.
git add -A
git diff --cached --check
committing_changes=0
if ! git diff --cached --quiet; then
  committing_changes=1
  while IFS= read -r path; do
    lower="$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')"
    if [[ "$lower" == ".env.example" || "$lower" == */.env.example ]]; then continue; fi
    if [[ "$lower" =~ (^|/)(\.env($|\.)|id_rsa|id_ed25519|auth\.json|credentials\.json|\.npmrc|\.pypirc) ]] || [[ "$lower" =~ \.(pem|p12|pfx|key)$ ]]; then
      echo "Sensitive-looking staged file: $path. Refusing push." >&2; exit 1
    fi
  done < <(git diff --cached --name-only)

  git diff --cached --stat
  printf '\nStaged files:\n'
  git diff --cached --name-only
  printf '\n==> commit\n'
  git commit -m "$message"
  sha="$(git rev-parse HEAD)"
  printf '\n==> push feature branch\n'
  git push -u origin "$branch"
else
  sha="$(git rev-parse HEAD)"
  printf 'No new changes to commit; checking whether verified commit %s is already pushed.\n' "$sha"
fi

# A clean-tree retry is allowed only when this exact, freshly verified commit is on
# the validated origin branch. For a new commit, this also confirms push delivery.
remote_sha="$(git ls-remote --heads origin "refs/heads/$branch" | awk 'NR == 1 { print $1 }')"
if [[ "$remote_sha" != "$sha" ]]; then
  if [[ "$committing_changes" == "0" ]]; then
    echo "No changes to commit and HEAD is not already pushed to origin/$branch; refusing no-op delivery." >&2
  else
    echo "Push did not publish the expected commit to origin/$branch; refusing pull-request creation." >&2
  fi
  exit 1
fi

printf '\n==> create or locate pull request\n'
# Include local evidence and the exact tree contents in a new PR description.
body_file=".thrn-reports/$branch_slug-pr.md"
changed_files="$(git diff-tree --no-commit-id --name-only -r "$sha")"
{
  printf '## Implementation summary\n\n- %s\n\n' "$message"
  printf '## Verification evidence\n\n- `bash scripts/verify.sh` — PASS (exit 0).\n- Verified commit: `%s`.\n' "$sha"
  printf '\n### Files in the verified commit\n\n'
  if [[ -n "$changed_files" ]]; then
    while IFS= read -r path; do printf -- '- `%s`\n' "$path"; done <<< "$changed_files"
  else
    printf 'No files in this commit.\n'
  fi
  printf '\n## Unresolved issues\n\n- GitHub Actions checks must be inspected after the pull request is created; pending checks are not a pass.\n'
} > "$body_file"

# Query all states so retries or reruns never create a duplicate PR.
pr_url="$(gh pr list --state all --head "$branch" --json url --jq '.[0].url // empty')"
if [[ -n "$pr_url" ]]; then
  printf 'Reusing existing pull request: %s\n' "$pr_url"
else
  pr_url="$(gh pr create --draft --base main --head "$branch" --title "$pr_title" --body-file "$body_file")"
fi

printf '\n=== DELIVERY REPORT ===\n'
printf 'branch: %s\ncommit: %s\npull_request: %s\nlocal_checks: PASS\nlocal_log: %s\n' "$branch" "$sha" "$pr_url" "$logfile"
printf 'changed_files:\n'
git diff-tree --no-commit-id --name-only -r "$sha"
printf '\nRemote checks must be independently confirmed; pending is not success.\n'
printf 'Review using: gh pr diff "%s" and gh pr checks "%s"\n' "$pr_url" "$pr_url"
