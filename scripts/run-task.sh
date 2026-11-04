#!/usr/bin/env bash
# Orchestrated mode: branch -> Codex CLI -> gate -> commit -> push -> draft PR.
# Interactive mode creates a branch for work already happening in the Codex GUI.
# This script deliberately runs git/gh OUTSIDE the agent sandbox.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if [[ "${1:-}" == "--interactive" ]]; then
  [[ "$#" -eq 2 ]] || { echo 'Usage: bash scripts/run-task.sh --interactive chore/name' >&2; exit 2; }
  branch="$2"
  bash scripts/start-task.sh "$branch"
  printf 'Interactive task branch ready: %s\n' "$branch"
  printf 'Implement the task in the Codex GUI, then run bash scripts/finish-task.sh "type: description".\n'
  exit 0
fi
[[ "$#" -eq 3 || "$#" -eq 4 ]] || { echo 'Usage: bash scripts/run-task.sh PROMPT.md feature/name "type: message" ["PR title"]' >&2; exit 2; }
prompt="$1"; branch="$2"; message="$3"
pr_title="${4:-$message}"
[[ -f "$prompt" ]] || { echo "Prompt not found: $prompt" >&2; exit 1; }
command -v codex >/dev/null || { echo 'Codex CLI missing; use --interactive from the Codex GUI or see START_HERE.md.' >&2; exit 1; }
bash scripts/start-task.sh "$branch"
echo 'Codex is implementing the task with workspace filesystem restrictions.'
# Select CODEX_MODEL if you want a specific model; otherwise use your Codex default.
if [[ -n "${CODEX_MODEL:-}" ]]; then
  codex exec --model "$CODEX_MODEL" --sandbox workspace-write - < "$prompt"
else
  codex exec --sandbox workspace-write - < "$prompt"
fi
# Failed Codex run ends before commit. Verification is deterministic, separate from agent narrative.
bash scripts/finish-task.sh "$message" "$pr_title"
