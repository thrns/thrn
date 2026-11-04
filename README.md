# THRN

A local-first, universal Rust developer CLI. Initial product: securely manage Agent Skills sourced from immutable public GitHub revisions.

**Project status:** bootstrap only, not a released installer. `src/main.rs` is intentionally a placeholder until M0-01. No npm/PyPI names or domains are claimed.

1. Read [START_HERE.md](START_HERE.md) and [THRN_PROJECT_SPEC.md](THRN_PROJECT_SPEC.md).
2. Run the Mac setup instructions, then `bash scripts/bootstrap-repo.sh` once.
3. Use `bash scripts/run-task.sh docs/prompts/M0-01.md feature/m0-foundation "feat: build initial Rust CLI"`.
4. Share the pull-request URL and latest commit SHA for independent review.

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery and the pre-push hook verifies the commit again.

If the branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. If there are new changes, those changes must pass verification before a commit or push.

The initial product must never run downloaded skill instructions during installation. Release is explicitly out of scope of the bootstrap.
