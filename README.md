# THRN

A local-first, universal Rust developer CLI. Initial product: securely manage Agent Skills sourced from immutable public GitHub revisions.

**Project status:** bootstrap only, not a released installer. `src/main.rs` is intentionally a placeholder until M0-01. No npm/PyPI names or domains are claimed.

1. Read [START_HERE.md](START_HERE.md) and [THRN_PROJECT_SPEC.md](THRN_PROJECT_SPEC.md).
2. Run the Mac setup instructions, then `bash scripts/bootstrap-repo.sh` once.
3. Use `bash scripts/run-task.sh docs/prompts/M0-01.md feature/m0-foundation "feat: build initial Rust CLI"`.
4. Share the pull-request URL and latest commit SHA for independent review.

The initial product must never run downloaded skill instructions during installation. Release is explicitly out of scope of the bootstrap.
