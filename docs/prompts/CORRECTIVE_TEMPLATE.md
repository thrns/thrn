# Correction for <task ID> — PR <number>, head <SHA>

The independent architect review found a blocking issue. Inspect the existing branch and diffs; do not expand scope.

**Blocking finding:** <file:line and actual behavior>
**Why it fails:** <exact spec clause or security invariant>
**Reproduction:** `<command or fixture>`
**Required correction:** <smallest accurate fix>
**Required regression test:** <test demonstrating failure before and pass after>
**Keep unchanged:** <unrelated files/decisions>

Re-run `bash scripts/verify.sh`, state exact results and files. Do not commit/push/merge; the orchestrator handles Git. No new architecture decisions without approval. Updated SHA needs a new independent review.
