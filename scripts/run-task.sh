#!/usr/bin/env bash
# One command: create branch -> invoke implementation agent -> gate -> commit -> push -> draft PR.
# This script deliberately runs git/gh OUTSIDE the agent sandbox.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ "$#" -eq 3 ]] || { echo 'Usage: bash scripts/run-task.sh PROMPT.md feature/name "feat: message"' >&2; exit 2; }
prompt="$1"; branch="$2"; message="$3"
[[ -f "$prompt" ]] || { echo "Prompt not found: $prompt" >&2; exit 1; }
command -v codex >/dev/null || { echo 'Codex CLI missing; see START_HERE.md.' >&2; exit 1; }
bash scripts/start-task.sh "$branch"
echo 'Codex is implementing the task with workspace filesystem restrictions.'
# Select CODEX_MODEL if you want a specific model; otherwise use your Codex default.
if [[ -n "${CODEX_MODEL:-}" ]]; then
  codex exec --model "$CODEX_MODEL" --sandbox workspace-write - < "$prompt"
else
  codex exec --sandbox workspace-write - < "$prompt"
fi
# Failed Codex run ends before commit. Verification is deterministic, separate from agent narrative.
bash scripts/finish-task.sh "$message"
