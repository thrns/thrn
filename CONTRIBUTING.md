# Contributing

Read `THRN_PROJECT_SPEC.md`, `AGENTS.md`, and `docs/ROADMAP.md` before proposing a change. Use one scoped branch (`feature/...`, `fix/...`) per task. From the Codex GUI, use `bash scripts/run-task.sh --interactive chore/task-name`; from the Codex CLI, use the prompt-driven `run-task.sh` mode. Both paths use `bash scripts/finish-task.sh` for verification and delivery. Submit a draft pull request against `main`, keep CI green, and do not merge until independent review accepts the stated criteria. Never commit secrets or execute untrusted downloaded skills.
