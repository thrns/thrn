# Independent verification and handoff protocol

Codex and the local script are **not independent reviewers**. Passing local checks is necessary, not sufficient. CI on GitHub-hosted runners provides separate execution evidence; architectural review must inspect the actual implementation.

## Delivery contract (from automation)

Supply: PR URL (or owner/repo + PR number), `git rev-parse HEAD` full SHA, branch, task ID, stated acceptance criteria, changed file list, exact local test commands/results, CI checks and run URLs, unresolved risk. `.thrn-reports/*.log` is local-only and intentionally ignored; do not publish private paths or secrets in logs.

## Review commands (read-only)

```bash
gh pr view PR_NUMBER --repo OWNER/thrn --json title,body,headRefName,headRefOid,baseRefName,url
gh pr diff PR_NUMBER --repo OWNER/thrn
gh pr checks PR_NUMBER --repo OWNER/thrn --watch
gh run list --repo OWNER/thrn --branch feature/BRANCH --limit 5
```

From ChatGPT, provide the PR URL and SHA in the next turn, with GitHub connected and repository access granted, or supply the diff/check logs if unavailable. ChatGPT's GitHub connection can inspect PRs/commits/files/checks when authorized, but it cannot automatically monitor every Codex completion in this ordinary chat.

## Reviewer checklist

1. Match **actual head SHA** to claimed SHA and PR branch; reject stale review evidence after a force-less follow-up commit.
2. Inspect changed files and diff; ensure no unrelated changes, new secrets, unsafe permissions or unapproved dependencies.
3. Trace acceptance criteria to code and tests. Check negative/security cases, not only nominal flows.
4. Inspect CI run status, runner platforms, tests invoked and any suspicious skipping/cancellation. Failed/pending CI is **not accepted**.
5. Review path traversal/ownership/rollback contracts when relevant, portability, network/timeout boundaries, error semantics and no implicit execution.
6. Compare with `THRN_PROJECT_SPEC.md`, `AGENTS.md`, `docs/DECISIONS.md`, and precedent PRs; flag architectural drift.
7. Conclude **ACCEPT**, **REJECT**, or **BLOCKED** and cite concrete files/lines and a reproduction command. Record the accepted SHA.
8. If REJECT, issue a targeted correction task on the same branch. Repeat CI and review on the updated SHA before accepting.
9. After ACCEPT, only an authorized user merges the PR; an AI response is not a GitHub review approval and cannot count as another human reviewer.

## Report template

```
Task: M0-01
Decision: ACCEPT | REJECT | BLOCKED
PR: https://github.com/OWNER/thrn/pull/NUMBER
Head SHA: 40-hex...
Spec requirements checked: §...
Acceptance criteria: [pass/fail per item]
CI evidence: job name / conclusion / run URL
Code observations: file:line, severity, rationale
Security observations: ...
Unresolved blockers: ...
Corrective actions (if required): ...
Next task (if accepted): ...
```

## Optional automatic Codex PR review

Codex supports native GitHub automatic reviews. Connect the GitHub repository to Codex in Codex settings, enable `Review code → Automatic review`, and select a trigger. This is a separate AI reviewer that can run on PRs without new user turns; it does not take over the independent ChatGPT architect role or replace mandatory CI or acceptance evidence. The implementation agent itself is not independent.

## Automatable vs review-owned

- **Local orchestrator**: branch creation, agent invocation, deterministic formatting/lint/tests/build, credential-bearing Git push, draft PR creation.
- **GitHub Actions**: fresh checkout, cross-platform compilation/test, RustSec dependency advisory audit. Cannot prove runtime safety or spec compliance.
- **Human architect / ChatGPT reviewer**: inspect code, actual tests and GitHub statuses; decide acceptance, architecture changes and next prompt. This is *turn-triggered* unless an external integration is configured.
- **Merge**: deliberately not automated until acceptance. A sole maintainer should set required PR and checks, but not require an impossible second human approval. If an additional reviewer exists, add mandatory review in GitHub rulesets.
