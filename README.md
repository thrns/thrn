# THRN

THRN is a local-first Rust command-line tool for developer resources.

**Current status:** the initial CLI provides help, version information, and a read-only `doctor` command. Skill discovery, downloads, installation, updates, and package distribution are not implemented or released.

## Build and run

```sh
cargo run --locked -- --help
cargo run --locked -- --version
cargo run --locked -- doctor
```

`thrn doctor` reports the crate version and the operating system and architecture reported by the build. That OS/architecture pair is not a full Rust target triple. The command does not inspect agent installations, access a registry, write files, or make network requests.

## CLI output and exit codes

Successful command output, including `--help`, `--version`, and `doctor`, goes to stdout. Clap usage errors and command diagnostics go to stderr. Successful commands exit with code `0`; invalid or missing command-line arguments exit with code `2`; output or other command I/O failures exit nonzero (currently code `1`). The CLI does not yet provide JSON output or a broader categorized failure-code scheme.

Use `thrn --help` to see the commands currently available. No npm or PyPI package is provided.

## Implementation notes

- [Architecture](docs/architecture.md)
- [CLI contract](docs/cli.md)
- [Security model](docs/security-model.md)

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery, and the pre-push hook verifies the commit again.

If a branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. New changes must pass verification before a commit or push.

The initial product must never run downloaded skill instructions during installation. No npm or PyPI package is provided.
