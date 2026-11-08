# THRN

THRN is a local-first Rust command-line tool for developer resources.

**Current status:** the CLI provides help, version information, a read-only `doctor` command, offline catalog listing/search/info with an explicitly triggered catalog refresh/cache, receipt-backed THRN-managed installation listing, local Agent Skills metadata validation, Codex project installation, and safe receipt-backed Codex project skill update and removal. Other-agent support and npm/PyPI packages are not provided.

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
cargo run --locked -- skills install frontend --agent codex --dry-run
cargo run --locked -- skills install frontend --agent codex --project ./my-project --dry-run
cargo run --locked -- skills install frontend-design --agent codex
cargo run --locked -- skills install frontend-design --agent codex --project ./my-project
cargo run --locked -- skills remove uncharted --agent codex --dry-run
cargo run --locked -- skills remove uncharted --agent codex
cargo run --locked -- skills remove uncharted --agent codex --project ./my-project
cargo run --locked -- skills update uncharted --agent codex --dry-run
cargo run --locked -- skills update uncharted --agent codex
cargo run --locked -- skills update uncharted --agent codex --project ./my-project
```

`thrn doctor` reports the crate version and the operating system and architecture reported by the build. That OS/architecture pair is not a full Rust target triple. The command does not inspect agent installations, access a registry, write files, or make network requests.

`thrn skills search [query]` and `thrn skills info <id>` never make network requests. They use the newest valid local catalog cache when available and otherwise use the catalog embedded in the executable. Output identifies the selected source and freshness. A cached catalog is fresh for 24 hours after a successful refresh; older cached data remains usable offline and is labeled stale. No command refreshes stale data automatically.

`thrn skills list` lists catalog availability, sorted by canonical catalog ID, with each skill's display name and description. It reports whether the source is the validated local cache or bundled catalog and labels cache freshness. `thrn skills search` remains available, including its no-query behavior of displaying all catalog entries.

`thrn skills list --installed` reads validated schema-v1 and schema-v2 receipts from the OS application-data directory's `receipts` subdirectory. New installs use v2 and record the committed destination's filesystem identity; historical v1 receipts remain listable. Missing or empty storage reports no THRN-managed installations. Valid records show canonical resource ID, skill name, agent, scope, project destination, and pinned revision; output says current content integrity was not checked. Listing never creates receipt storage, fetches the catalog, or opens project destinations. Corrupt, conflicting, unsupported, linked, or unknown receipt objects fail closed. Skills installed manually or by other tools are not examined. `THRN_DATA_DIR` may point to an alternate absolute application-data directory for isolation; it is separate from `THRN_CACHE_DIR`. See [receipt design](docs/receipts.md).

`thrn skills search --refresh [query]`, install/update dry-runs, and real installs/updates can make network requests. Refresh fetches the fixed THRN-operated HTTPS endpoint `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`, validates it, then atomically replaces the local cache. Install and update use the newest valid local catalog or bundled metadata without refreshing it, then fetch the selected skill archive from its exact pinned GitHub commit. The source is validated and staged in a private temporary directory. The cache uses the operating-system cache directory (`$XDG_CACHE_HOME/thrn` or `~/.cache/thrn` on Linux, `~/Library/Caches/im.thrn.thrn` on macOS, and `%LOCALAPPDATA%/thrn/thrn/cache` on Windows). `THRN_CACHE_DIR` may point to an alternate absolute cache directory; it does not change the catalog source. If the cache is missing or invalid, discovery falls back to the bundled catalog. A failed refresh reports an error and preserves the previous cache. HTTPS protects transport to the configured endpoint; endpoint curation, HTTPS, catalog inclusion, and a pinned Git commit do not establish the identity or safety of individual skill publishers. A catalog version of `unversioned` means that no upstream skill version is present in curated metadata. See [the registry documentation](docs/registry.md) for cache details and examples.

`thrn skills install <id> --agent codex --dry-run` prints a read-only plan. `thrn skills install <id> --agent codex` installs the validated files. In both cases, the project defaults to the current directory; `--project <path>` selects another existing directory. The destination is `<canonical-project-root>/.agents/skills/<skillName>`, using the validated skill name rather than a catalog ID or alias. The dry-run lists planned directories and regular files without modifying the project. A real install reuses the validated staged content, rechecks the destination, commits the complete directory without replacement, and records a durable ownership receipt. Success is reported only after the journal and receipt operations complete. A failure after directory commitment can leave installed files without a completed receipt; the error states that the operation is incomplete, and the journal supports recovery. Never manually delete a receipt, journal, or installed directory to resolve an error. Duplicate managed installs and existing unreceipted skills are preserved and rejected.

`thrn skills remove <canonical-id> --agent codex` removes one project installation only when its exact canonical ID has a valid schema-v2 receipt with installation-time destination identity and the recorded destination, directories, files, byte counts, and SHA-256 digests all match current content. Historical schema-v1 receipts remain listable but automatic removal refuses them because they cannot prove installation-time directory ownership. Removal does not read or refresh the catalog. `--dry-run` lists the exact destination and all receipt-listed files without writing project, receipt, or journal data. Removal uses the receipt-store writer lock, a no-replace sibling quarantine, and a separate versioned recovery journal. Modified, missing, extra, linked, hard-linked, or unmanaged content is preserved and reported as a conflict. A partial transaction returns nonzero and recovery resumes only when ownership remains provable; preserve its content, receipt, and journal for inspection. Removal leaves `.agents` and `.agents/skills` in place. It does not run scripts or other installed content. `skills list --installed` stops listing the installation after durable removal and receipt retirement. See [receipt design](docs/receipts.md) and [recovery behavior](docs/recovery.md).

`thrn skills update <canonical-id> --agent codex` replaces one exactly verified THRN-managed project skill with the newer full commit pinned by the current validated local catalog. It requires the same resource ID, GitHub owner/repository, source directory, installed skill name, project identity, schema-v2 receipt, and complete current tree. Modified, extra, missing, replaced, linked, legacy, or ambiguous content is preserved and rejected. `--dry-run` may fetch the exact pinned source to compare files, but it does not modify the project or receipt store. A same-revision update succeeds with an “Already up to date” result and makes no changes. The real update keeps the old directory and receipt in operation-owned backups until the new directory and receipt verify; interrupted transactions block installed listing and are recovered under the shared writer lock. Recovery never adopts files based only on hashes. Cross-filesystem power-loss atomicity is not claimed, and Windows directory-entry durability follows the existing secure-handle limitation. See [receipt design](docs/receipts.md), [recovery behavior](docs/recovery.md), and proposed [ADR-014](docs/DECISIONS.md#adr-014-receipt-backed-skill-update-transaction).

`thrn skills validate <path>` reads only the regular, UTF-8 `SKILL.md` in the supplied directory. It checks the official frontmatter fields, YAML structure, name grammar and directory match, and documented size and parser limits. It does not inspect supporting files, modify content, make network requests, or execute scripts. A passing metadata check does not establish source authenticity or make any included scripts safe. See [the validation rules and limits](docs/registry.md#local-agent-skills-validation).

## CLI output and exit codes

Successful command output, including empty catalog and managed-installation states, goes to stdout and exits with code `0`. Clap usage errors, including missing required arguments or unknown options, go to stderr and exit with code `2`. Invalid lookup identifiers, unknown catalog IDs, corrupt or unsupported receipts, ownership conflicts, and command I/O failures go to stderr and exit with code `1`. The CLI does not yet provide JSON output or a broader categorized failure-code scheme.

Use `thrn --help` to see the commands currently available. The command currently supports only explicit `--agent codex` and project scope. No npm or PyPI package is provided.

## Implementation notes

- [Architecture](docs/architecture.md)
- [CLI contract](docs/cli.md)
- [Security model](docs/security-model.md)
- [Installation receipts](docs/receipts.md)

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery, and the pre-push hook verifies the commit again.

If a branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. New changes must pass verification before a commit or push.

The CLI does not execute skill instructions. No npm or PyPI package is provided.
