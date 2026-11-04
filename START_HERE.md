# THRN — macOS setup and first implementation cycle

**Checked:** Nov 4, 2026. **This zip is a local starter, not an already-running GitHub repository.** No real Cargo/Codex/GitHub commands were executed by the authoring environment: the provided scripts must run on your Mac. Read `THRN_PROJECT_SPEC.md` (authoritative), `AGENTS.md`, `docs/ROADMAP.md`, `docs/REVIEW_PROTOCOL.md`.

## A. Install the toolchain (once)

Run in macOS Terminal (Apple Silicon or Intel). Skip any existing working tool. Inspect any downloaded install script if you require supply-chain review.

```bash
xcode-select --install                          # macOS Command Line Tools; skip if installed
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"  # only if brew missing
# After installation, follow Homebrew's printed shellenv command, then:
brew install git gh node
brew install --cask visual-studio-code

# Rustup official installer; can inspect /tmp/rustup-init.sh before executing.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
sh /tmp/rustup-init.sh -y --profile minimal
source "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt --component clippy

# Codex: npm distribution as documented by OpenAI.
npm install -g @openai/codex

# VS Code Command Palette -> Shell Command: Install 'code' command in PATH, if needed.
code --install-extension rust-lang.rust-analyzer
code --install-extension vadimcn.vscode-lldb
code --install-extension tamasfe.even-better-toml
code --install-extension OpenAI.chatgpt

# Verify tools (versions may be newer; Rust is pinned by repository file).
git --version
gh --version
node --version
cargo --version
rustc --version
codex --version
```

If Homebrew installation has already modified your shell configuration, skip its install line. macOS `bash` is older, so scripts deliberately use features compatible with the default macOS Bash 3.2. You do **not** need Python, uv, Maturin, Node packaging, or `dist` to implement M0; install release tooling in M4–M5.

## B. Authenticate once (no tokens in the repository)

```bash
gh auth login --web --git-protocol https
gh auth setup-git
gh auth status
git config --global user.name "YOUR_GIT_AUTHOR_NAME"
git config --global user.email "YOUR_GITHUB_EMAIL_OR_NOREPLY"
```

Run `codex` once and sign in using the ChatGPT option. In the Codex IDE sidebar, choose a model available to your account. `gpt-6-luna` is documented for focused coding; `gpt-6.1-sol` is documented for longer complex tasks if your account has it. For the command-line orchestrator, optionally choose `export CODEX_MODEL=gpt-6-luna`. The default model will be used if `CODEX_MODEL` is unset. Do not place credentials in `config.toml`, `.env`, prompts, commit messages, or local logs.

## C. Extract starter and initialize local Git

```bash
mkdir -p ~/Developer
cd ~/Developer
unzip ~/Downloads/thrn-starter.zip
cd thrn-starter
# Optional rename to your final project folder **before** initializing:
cd .. && mv thrn-starter thrn && cd thrn

bash scripts/verify.sh       # requires the pinned Rust toolchain
bash scripts/bootstrap-repo.sh
code .
```

The bootstrap script runs verification, creates a local `main` branch, commits the starter, and installs a repository-local pre-push hook. It never accesses your remote GitHub account. If you already have a THRN repository with work in it, **do not bootstrap a second repo**: clone your existing repo and transfer/reconcile these files on a dedicated setup branch instead.

## D. Create GitHub repository and enforce branch safety

If no remote exists, this is a consequential one-time publication action **you run yourself**:

```bash
gh repo create thrn --public --source=. --remote=origin --push
# Alternatively choose --private if your GitHub plan supports the protections you need.
gh repo view --web
```

For GitHub Free, public repos support branch rulesets; private repo branch rulesets may require a paid plan. Configure **Settings → Rules → Rulesets → New branch ruleset**, target `main`, enforcement `Active`, and:

- Require a pull request before merging; no direct pushes / deletions / force pushes.
- Require successful status checks (exact CI job names once a workflow has run): `quality`, `advisory`, `cross-platform (macos-latest)`, `cross-platform (windows-latest)`.
- Require branches up to date before merging when practical.
- Require linear history if you prefer squash/rebase merges; disable bypasses and maintain broad protections for admins where supported.
- For a **single-maintainer** repo, don't require approval from a *different GitHub account* unless a real second reviewer exists: ChatGPT independent verification is not a GitHub review approval. Require the PR and CI; merge manually only after architectural acceptance here.
- Enable GitHub secret scanning/push protection if available, dependency alerts, Dependabot and default read-only `GITHUB_TOKEN` permissions.

**Optional automatic second-pass review:** Connect this repository to Codex's GitHub integration in [Codex settings](https://chatgpt.com) and enable **Review code → Automatic review** for this repo, plus the applicable personal review trigger. This is an actual supported GitHub integration, not a feature of `AGENTS.md` or `run-task.sh` itself. It reviews incoming PRs without requiring you to comment `@codex review` each time. Reviews emphasize significant findings; they do not replace this chat's architecture acceptance and cannot ensure the product specification is satisfied. Security Review is separately entitlement-gated.

Do not activate status checks before the named jobs have run at least once. The first feature PR will create them; then select them as required before any merge. This is a one-time ruleset setup. Our local pre-push hook blocks ordinary direct pushes to established `main` but is not a replacement for server-side rules.

## E. First implementation — ONE command

From `~/Developer/thrn`:

```bash
# Optional: choose GPT-6 Luna for focused, efficient tasks.
export CODEX_MODEL=gpt-6-luna

bash scripts/run-task.sh \
  docs/prompts/M0-01.md \
  feature/m0-foundation \
  "feat: establish real Rust CLI foundation"
```

The script 1) verifies a clean worktree, 2) fetches latest main, 3) creates `feature/m0-foundation`, 4) calls `codex exec` in a workspace-write sandbox with the self-contained prompt, 5) runs Cargo format/lint/tests/release build, 6) checks sensitive-looking staged paths, 7) commits and pushes **only after local checks pass**, and 8) creates a **regular** PR. It prints the branch, head SHA, PR URL, changed files and local verification log location. The script never merges or publishes a release. `CODEX_MODEL` is optional; authorization or sandbox restrictions may still require intervention.

The independent GitHub-runner checks are separate and may be pending when the script exits. After the command completes:

```bash
gh pr checks --watch             # from the feature branch
gh pr view --web
```

Share the PR URL and full SHA in this ChatGPT conversation and ask the architect to review M0-01. The GitHub connector must have read access to the correct repo (or supply the diff/check logs). I will inspect source, diffs, and CI evidence and either accept that exact SHA or return a corrective prompt. **I cannot automatically read your VS Code workspace or receive a Codex completion event in this chat.**

## F. Subsequent tasks and corrections

For the next architect-approved task, place the provided prompt under `docs/prompts/` or use a path outside the repo, then run:

```bash
bash scripts/run-task.sh docs/prompts/M0-02.md feature/m0-cli-errors "test: harden CLI error semantics"
```

For a correction on the **existing branch**, do not run `start-task` again:

```bash
codex exec --sandbox workspace-write - < docs/prompts/correction-M0-01.md
bash scripts/finish-task.sh "fix: address M0-01 review findings"
```

From the VS Code Codex sidebar, you can execute a prompt interactively and then run `bash scripts/finish-task.sh "feat: ..."` from its existing task branch. This path does not automatically intercept every IDE chat: the one-command fully orchestrated path is the `run-task.sh` CLI mode.

## G. Trust/security boundaries and troubleshooting

- The orchestrator runs local code/tests and has access to your local Git credentials. Only run on repositories you trust; untrusted Rust build scripts/dependencies can execute code when tested. For stronger isolation, use a disposable dedicated VM/container with scoped credentials.
- `workspace-write` and project network access permit dependency download but do not guarantee that all remote/sensitive actions can happen without approval. Git and GitHub operations intentionally run outside the agent sandbox in the wrapper script, not through broad `--yolo` access.
- A failing `bash scripts/verify.sh` or nonzero Codex result prevents the scripted commit/push. Git hook repeats verification on pushes. Branch protections and CI are an additional barrier, not an infallible proof of behavior.
- If you cannot reach GitHub, crates.io, or Codex authentication, fix connectivity/credentials and rerun verification. Failed runs may leave a task branch with edits; inspect them, continue in place, and use `finish-task.sh` rather than recreating the branch.
- `cargo fmt` checks source formatting; run `cargo fmt --all` to repair formatting; never change rules just to obtain green checks.
- Current bootstrap has no external runtime environment variables. Do not use personal access tokens unless the secure `gh` browser flow is unavailable.
- Pending or failed GitHub CI means the task remains **unaccepted**, even if local checks were green. Review must be repeated for each new SHA.

## H. Work split

**Architect:** chooses next task, writes prompt, evaluates real PR/CI, approves changes and next stage. **Codex:** edits and tests code only. **Scripts:** create branch, run implementation, verify, commit, push, open PR. **GitHub Actions:** independent platform checks. **You:** one-time authentication/repository protection, initiate each run, provide PR URL for independent ChatGPT review, and merge only after accepted.

Official reading and source links: `docs/SOURCES.md`.
