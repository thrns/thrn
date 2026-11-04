# Task <ID> — <title>

Read `AGENTS.md`, relevant clauses in `THRN_PROJECT_SPEC.md`, `docs/DECISIONS.md`, and actual source/tests. The architect owns all major choices. The orchestrator owns Git actions.

**Branch:** `feature/<task-slug>` (never contains the implementing agent's name).
**Scope:** Only this task. Do not begin successor milestones.
**Depends on:** <accepted task IDs and SHAs>.
**Specification:** <file sections, contract>.

## Exact implementation instructions

<concrete modules/functions, boundaries, dependencies, migration constraints>

## Acceptance tests

1. <observable positive behavior and command>
2. <negative/security behavior and command>
3. <platform/edge behavior>
4. `bash scripts/verify.sh` passes
5. CI passes on the PR commit; reported separately after push

## Out of scope

<specific features not to implement>

## Completion

Run all relevant tests, fix failures. Report branch, task ID, changed files, exact commands/results, unmet criteria and risks. **Do not commit/push/merge**; the orchestration script handles commits and feature-branch pushes.
