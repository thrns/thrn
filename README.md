# THRN

THRN is a local-first Rust command-line tool for developer resources.

**Current status:** the CLI provides help, version information, a read-only `doctor` command, and offline search and info for a small bundled skill catalog. Skill content is not downloaded or installed; updates, runtime catalog fetching, and package distribution are not implemented or released.

## Build and run

```sh
cargo run --locked -- --help
cargo run --locked -- --version
cargo run --locked -- doctor
cargo run --locked -- skills search
cargo run --locked -- skills search frontend
cargo run --locked -- skills info frontend
cargo run --locked -- skills info frontend-design
```

`thrn doctor` reports the crate version and the operating system and architecture reported by the build. That OS/architecture pair is not a full Rust target triple. The command does not inspect agent installations, access a registry, write files, or make network requests.

`thrn skills search` and `thrn skills info` read curated metadata from a static catalog embedded in the executable. The current catalog includes `frontend-design`, with the explicit alias `frontend`. Search without a query lists all bundled entries; text queries match IDs, aliases, skill directory names, display names, and descriptions without case sensitivity. Info accepts only a canonical catalog ID or an explicit alias and displays the curated source path and pinned commit. It does not fetch the source or download or install skill content. A pinned commit identifies a source revision; it does not guarantee publisher authenticity.

Catalog changes require shipping updated metadata in a new executable. Runtime fetching and caching are planned for M1-04. A catalog version of `unversioned` means the curated record has no upstream skill version metadata; it is not an invented upstream release number. See [the registry documentation](docs/registry.md) for the verified source revision and sample output.

## CLI output and exit codes

Successful command output, including search with no matches, goes to stdout and exits with code `0`. Clap usage errors, including missing required arguments, go to stderr and exit with code `2`. Invalid lookup identifiers, unknown catalog IDs, and command I/O failures go to stderr and exit with code `1`. The CLI does not yet provide JSON output or a broader categorized failure-code scheme.

Use `thrn --help` to see the commands currently available. No npm or PyPI package is provided.

## Implementation notes

- [Architecture](docs/architecture.md)
- [CLI contract](docs/cli.md)
- [Security model](docs/security-model.md)

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery, and the pre-push hook verifies the commit again.

If a branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. New changes must pass verification before a commit or push.

The CLI does not execute skill instructions. No npm or PyPI package is provided.
