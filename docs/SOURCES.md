# Official documentation verified November 4, 2026

- [OpenAI Codex CLI and installation](https://learn.chatgpt.com/docs/codex/cli): install CLI, ChatGPT sign-in, CLI execution.
- [Codex IDE extension](https://learn.chatgpt.com/docs/codex/ide): VS Code setup and sidebar.
- [Codex CLI command reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli): `codex exec`, sandbox flags, model argument.
- [Codex `AGENTS.md`](https://learn.chatgpt.com/docs/agent-configuration/agents-md): project instruction discovery.
- [Codex config basics](https://learn.chatgpt.com/docs/config-file/config-basic): `.codex/config.toml` and permission modes.
- [Codex GitHub automatic PR review](https://learn.chatgpt.com/docs/third-party/github): supports automatic code review when the repo is connected and settings enabled.
- [Codex models](https://learn.chatgpt.com/docs/models?surface=cli): available model identifiers and rollout caveats.
- [VS Code Rust tooling](https://code.visualstudio.com/docs/languages/rust) and [VS Code Tasks](https://code.visualstudio.com/docs/editor/tasks).
- [GitHub CLI authenticate](https://cli.github.com/manual/gh_auth_login), [create repo](https://cli.github.com/manual/gh_repo_create), [PR create](https://cli.github.com/manual/gh_pr_create), [PR checks](https://cli.github.com/manual/gh_pr_checks).
- [GitHub repository rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets) and [ruleset availability](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets).
- [GitHub Actions secure use](https://docs.github.com/en/actions/reference/security/secure-use).
- [Rust official release 1.99.1 (Oct 15, 2026)](https://blog.rust-lang.org/releases/latest/).
- [Agent Skills standard](https://agentskills.io/specification).
- [clap derive](https://docs.rs/clap/latest/clap/_derive/), [RustSec audit](https://github.com/rustsec/rustsec/tree/main/cargo-audit).
- [dist](https://axodotdev.github.io/cargo-dist/), [Maturin binary wheels](https://www.maturin.rs/bindings), [uv tools](https://docs.astral.sh/uv/guides/tools/), [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/).

**Environment limitation:** The starter was authored in a sandbox without Rust/Cargo, GitHub CLI, or live GitHub credentials; it was syntax-checked where possible but not executed as a real project on macOS/GitHub. Reverify version selection and compatibility from current docs before each release milestone. Starter checkout action is SHA-pinned to a known v4.2.2 revision; update to a currently reviewed commit after verifying the exact SHA on GitHub.
