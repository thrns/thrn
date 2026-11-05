# THRN

THRN is a local-first Rust command-line tool for developer resources.

**Current status:** the CLI provides help, version information, a read-only `doctor` command, offline catalog listing, search and info with an explicitly triggered catalog refresh/cache, a read-only THRN-managed installation listing, and read-only local Agent Skills metadata validation. Skill content is not downloaded or installed; package distribution is not implemented or released.

## Build and run

```sh
cargo run --locked -- --help
cargo run --locked -- --version
cargo run --locked -- doctor
cargo run --locked -- skills search
cargo run --locked -- skills search frontend
cargo run --locked -- skills list
cargo run --locked -- skills list --installed
cargo run --locked -- skills search --refresh frontend
cargo run --locked -- skills info frontend
cargo run --locked -- skills info frontend-design
cargo run --locked -- skills validate ./tests/fixtures/skills/valid-skill
```

`thrn doctor` reports the crate version and the operating system and architecture reported by the build. That OS/architecture pair is not a full Rust target triple. The command does not inspect agent installations, access a registry, write files, or make network requests.

`thrn skills search [query]` and `thrn skills info <id>` never make network requests. They use the newest valid local catalog cache when available and otherwise use the catalog embedded in the executable. Output identifies the selected source and freshness. A cached catalog is fresh for 24 hours after a successful refresh; older cached data remains usable offline and is labeled stale. No command refreshes stale data automatically.

`thrn skills list` lists catalog availability, sorted by canonical catalog ID, with each skill's display name and description. It reports whether the source is the validated local cache or bundled catalog and labels cache freshness. `thrn skills search` remains available, including its no-query behavior of displaying all catalog entries.

`thrn skills list --installed` reports only THRN-managed installations recorded by THRN. At this milestone there is no installer or receipt writer, so missing or empty receipt storage is reported as no THRN-managed installations. Skills installed manually or by other tools are not examined. Existing nonempty receipt storage is an error until M2-05 defines the durable receipt schema; listing does not create directories or write files. Receipt discovery uses the OS application-data directory and its `receipts` subdirectory. `THRN_DATA_DIR` may point to an alternate absolute application-data directory for isolation; it is separate from `THRN_CACHE_DIR`.

Only `thrn skills search --refresh [query]` makes a network request. It fetches the fixed THRN-operated HTTPS endpoint `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`, validates the response with the same catalog parser used for bundled data, then atomically replaces the local cache. The cache uses the operating-system cache directory (`$XDG_CACHE_HOME/thrn` or `~/.cache/thrn` on Linux, `~/Library/Caches/im.thrn.thrn` on macOS, and `%LOCALAPPDATA%/thrn/thrn/cache` on Windows). `THRN_CACHE_DIR` may point to an alternate absolute cache directory; it does not change the catalog source. If the cache is missing or invalid, discovery falls back to the bundled catalog. A failed refresh reports an error and preserves the previous cache. HTTPS protects transport to the configured endpoint; endpoint curation, HTTPS, catalog inclusion, and a pinned Git commit do not establish the identity or safety of individual skill publishers. Refresh does not retrieve skill files. A catalog version of `unversioned` means that no upstream skill version is present in curated metadata. See [the registry documentation](docs/registry.md) for cache details and examples.

`thrn skills validate <path>` reads only the regular, UTF-8 `SKILL.md` in the supplied directory. It checks the official frontmatter fields, YAML structure, name grammar and directory match, and documented size and parser limits. It does not inspect supporting files, modify content, make network requests, or execute scripts. A passing metadata check does not establish source authenticity or make any included scripts safe. See [the validation rules and limits](docs/registry.md#local-agent-skills-validation).

## CLI output and exit codes

Successful command output, including empty catalog and managed-installation states, goes to stdout and exits with code `0`. Clap usage errors, including missing required arguments or unknown options, go to stderr and exit with code `2`. Invalid lookup identifiers, unknown catalog IDs, unsupported receipt content, and command I/O failures go to stderr and exit with code `1`. The CLI does not yet provide JSON output or a broader categorized failure-code scheme.

Use `thrn --help` to see the commands currently available. No npm or PyPI package is provided.

## Implementation notes

- [Architecture](docs/architecture.md)
- [CLI contract](docs/cli.md)
- [Security model](docs/security-model.md)

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery, and the pre-push hook verifies the commit again.

If a branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. New changes must pass verification before a commit or push.

The CLI does not execute skill instructions. No npm or PyPI package is provided.
