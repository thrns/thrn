# THRN

THRN is a local-first Rust command-line tool for developer resources.

**Current status:** the CLI provides help, version information, a read-only `doctor` command, offline catalog listing/search/info with an explicitly triggered catalog refresh/cache, receipt-backed THRN-managed installation listing and offline update checking, local Agent Skills metadata validation, and receipt-backed Codex, Claude Code, Cursor, and Gemini CLI project/user skill lifecycles. M3-06 and proposed ADR-017 await independent architectural review; no npm/PyPI packages are provided.

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
cargo run --locked -- skills install frontend-design --agent codex --scope user --dry-run
cargo run --locked -- skills install frontend-design --agent codex --scope user
cargo run --locked -- skills remove uncharted --agent codex --dry-run
cargo run --locked -- skills remove uncharted --agent codex
cargo run --locked -- skills remove uncharted --agent codex --project ./my-project
cargo run --locked -- skills remove uncharted --agent codex --scope user --dry-run
cargo run --locked -- skills remove uncharted --agent codex --scope user
cargo run --locked -- skills update uncharted --agent codex --dry-run
cargo run --locked -- skills update uncharted --agent codex
cargo run --locked -- skills update uncharted --agent codex --project ./my-project
cargo run --locked -- skills update uncharted --agent codex --scope user --dry-run
cargo run --locked -- skills update uncharted --agent codex --scope user
cargo run --locked -- skills check
cargo run --locked -- skills check --project ./my-project
cargo run --locked -- skills check --agent codex
cargo run --locked -- skills check --scope user
cargo run --locked -- skills install uncharted --agent claude-code --dry-run
cargo run --locked -- skills install uncharted --agent claude-code --scope user
cargo run --locked -- skills remove uncharted --agent claude-code --scope user --dry-run
cargo run --locked -- skills update uncharted --agent claude-code --scope user --dry-run
cargo run --locked -- skills check --agent claude-code --scope project
cargo run --locked -- skills check --agent claude-code --scope user
cargo run --locked -- skills install uncharted --agent cursor --dry-run
cargo run --locked -- skills install uncharted --agent cursor --scope user
cargo run --locked -- skills remove uncharted --agent cursor --scope user --dry-run
cargo run --locked -- skills update uncharted --agent cursor --scope user --dry-run
cargo run --locked -- skills check --agent cursor --scope user
cargo run --locked -- skills install uncharted --agent gemini --dry-run
cargo run --locked -- skills install uncharted --agent gemini --scope user
cargo run --locked -- skills remove uncharted --agent gemini --scope user --dry-run
cargo run --locked -- skills update uncharted --agent gemini --scope user --dry-run
cargo run --locked -- skills check --agent gemini --scope user
```

`thrn doctor` reports the crate version, operating system and architecture, and whether the `codex`, `claude`, `cursor`, and `gemini` command names are present on `PATH`. The OS/architecture pair is not a full Rust target triple. PATH inspection is read-only and informational: it does not execute binaries, create or inspect agent configuration/skill roots, select an install target, or make network requests.

`thrn skills search [query]` and `thrn skills info <id>` never make network requests. They use the newest valid local catalog cache when available and otherwise use the catalog embedded in the executable. Output identifies the selected source and freshness. A cached catalog is fresh for 24 hours after a successful refresh; older cached data remains usable offline and is labeled stale. No command refreshes stale data automatically.

`thrn skills list` lists catalog availability, sorted by canonical catalog ID, with each skill's display name and description. It reports whether the source is the validated local cache or bundled catalog and labels cache freshness. `thrn skills search` remains available, including its no-query behavior of displaying all catalog entries.

`thrn skills list --installed` reads validated Codex project schema-v1/v2, Codex user schema-v3, Claude Code schema-v4, and Cursor/Gemini schema-v5 receipts from the OS application-data directory's `receipts` subdirectory. Existing receipt keys and digest meanings are unchanged. New adapter receipts bind agent, scope, layout, canonical root and filesystem identity, destination identity, pinned source provenance, and the full content inventory. Valid records show canonical resource ID, skill name, agent, actual scope, that scope's destination, and pinned revision; output says current content integrity was not checked. Listing never creates receipt storage, fetches the catalog, or opens skill destinations. Corrupt, conflicting, unsupported, linked, or unknown receipt objects fail closed. Skills installed manually or by other tools are not examined. `THRN_DATA_DIR` may point to an alternate absolute application-data directory for isolation; it is separate from `THRN_CACHE_DIR`. See [receipt design](docs/receipts.md).

`thrn skills search --refresh [query]`, install/update dry-runs, and real installs/updates can make network requests. Refresh fetches the fixed THRN-operated HTTPS endpoint `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`, validates it, then atomically replaces the local cache. Install and update use the newest valid local catalog or bundled metadata without refreshing it, then fetch the selected skill archive from its exact pinned GitHub commit. The source is validated and staged in a private temporary directory. The cache uses the operating-system cache directory (`$XDG_CACHE_HOME/thrn` or `~/.cache/thrn` on Linux, `~/Library/Caches/im.thrn.thrn` on macOS, and `%LOCALAPPDATA%/thrn/thrn/cache` on Windows). `THRN_CACHE_DIR` may point to an alternate absolute cache directory; it does not change the catalog source. If the cache is missing or invalid, discovery falls back to the bundled catalog. A failed refresh reports an error and preserves the previous cache. HTTPS protects transport to the configured endpoint; endpoint curation, HTTPS, catalog inclusion, and a pinned Git commit do not establish the identity or safety of individual skill publishers. A catalog version of `unversioned` means that no upstream skill version is present in curated metadata. See [the registry documentation](docs/registry.md) for cache details and examples.

Install, update, and remove require an explicit agent. `skills check` defaults to Codex for backward compatibility. THRN does not auto-select an agent; a read-only PATH capability API can report whether a corresponding CLI binary is present but does not assert that a target path is loadable. Codex uses `<project>/.agents/skills/<skillName>` and `<home>/.agents/skills/<skillName>`; Claude Code uses `.claude/skills`; Cursor uses `.cursor/skills`; Gemini CLI uses `.gemini/skills`, relative to the project or validated home. Project scope is the default; `--project <path>` remains project-only. Explicit `--scope user` selects the validated OS home and cannot be combined with `--project`. The same resource and skill name can be managed independently for each agent and scope. THRN reuses the same validated source, staging, no-replace commit, ownership receipt, journal, lock, and recovery engine. Success is reported only after the installed tree and receipt verify. A failure after directory commitment can leave installed files without a completed receipt; the error states that the operation is incomplete, and the journal supports recovery. Never manually delete a receipt, journal, or installed directory to resolve an error. Duplicate managed installs and existing unreceipted skills are preserved and rejected.

Cursor and Gemini also discover the shared `.agents/skills` path. THRN checks it before a native install but never adopts, moves, links, clones, or deletes its content. A verified Codex receipt means that shared skill may already be discoverable, while update and removal remain Codex-owned. An unmanaged shared skill is also preserved. Gemini documents `.agents/skills` precedence over `.gemini/skills` within the same discovery tier, so a same-named shared skill blocks native installation. Cursor's documentation does not establish same-name precedence, so THRN reports the collision and blocks installation. See [proposed ADR-017](docs/DECISIONS.md#adr-017-cursor-and-gemini-cli-adapters-and-shared-discovery).

`thrn skills remove <canonical-id> --agent <codex|claude-code|cursor|gemini>` removes one exact project installation by default. Pass `--scope user` to select only that agent's user installation. Removal requires a matching scope-bound receipt, installation-time directory identity, and complete file inventory and hashes. Legacy Codex project schema-v1 receipts remain listable but cannot authorize removal. Removal does not read or refresh the catalog. `--dry-run` lists the exact destination and all receipt-listed files without changing skill or receipt data. All four agents and both scopes use the same writer lock, no-replace sibling quarantine, and versioned recovery journal. Modified, missing, extra, linked, hard-linked, or unmanaged content is preserved and reported as a conflict. Removal does not execute scripts or other installed content. It leaves agent parents in place and retires the receipt only after verified cleanup. See [receipt design](docs/receipts.md) and [recovery behavior](docs/recovery.md).

`thrn skills update <canonical-id> --agent <codex|claude-code|cursor|gemini>` updates the selected project installation by default; pass `--scope user` for that agent's user installation. The scope-bound receipt, source identity, installed skill name, root identity, and complete current tree must verify. Modified, extra, missing, replaced, linked, or ambiguous content is preserved and rejected. `--dry-run` may fetch the exact pinned source to compare files, but it does not modify the skill or receipt store. A same-revision update makes no changes. The shared update engine keeps the old directory and receipt in operation-owned backups until the new directory and receipt verify; interrupted transactions block listing and recover under the writer lock. Recovery never adopts files based only on hashes. Cross-filesystem power-loss atomicity is not claimed, and Windows directory-entry durability follows the existing secure-handle limitation. See [receipt design](docs/receipts.md), [recovery behavior](docs/recovery.md), proposed [ADR-014](docs/DECISIONS.md#adr-014-receipt-backed-skill-update-transaction), and proposed [ADR-017](docs/DECISIONS.md#adr-017-cursor-and-gemini-cli-adapters-and-shared-discovery).

`thrn skills check` checks Codex project receipts by default for backward compatibility. `--agent claude-code`, `--agent cursor`, or `--agent gemini` selects another adapter; `--scope user` checks only the selected agent's user receipts under the validated home. Use `--project <path>` only with project scope. The command is offline-only: it does not refresh the catalog, contact GitHub, execute skill content, create application-data directories or lock files, change receipts or journals, or run recovery. It takes an existing writer lock briefly when present and fails if the store is busy or a lifecycle journal is pending. An empty selected scope succeeds with a clear message.

The check reports **Up to date** when the scope-appropriate receipt identity and installed tree match the local catalog pin; **Update available** when the tree and update identity verify but the catalog pins a different full SHA; **Not in catalog** when the canonical ID is absent; **Source mismatch** when the catalog changes the owner, repository, source directory, or skill name; **Legacy receipt** when a verified schema-v1 receipt lacks installation-time directory identity; and **Modified or unsafe installation** when the recorded directory identity, contents, or structure do not verify. Modified, unsafe, legacy, and source-mismatched installations are not eligible for automatic update. Check status is informational and exits `0` even when an update is available or an ID is no longer cataloged. Data, filesystem, catalog, or recovery errors exit `1`; CLI usage errors exit `2`.

The displayed catalog SHA is the selected local pin. A different SHA means the catalog points at a different pinned revision; it does not prove that Git considers the commit newer or that it descends from the installed commit. The header identifies whether THRN used the local cache or bundled metadata and whether cache freshness is known. A stale cache or bundled catalog is not authoritative latest GitHub information. After reviewing an **Update available** finding, explicitly run `thrn skills update <canonical-id> --agent <codex|claude-code|cursor|gemini>`; update rechecks ownership and integrity and may fetch the catalog-pinned GitHub archive. A differing SHA alone does not make a modified or legacy installation eligible. See the [CLI contract](docs/cli.md#read-only-update-check) and [security model](docs/security-model.md#read-only-update-checking).

`thrn skills validate <path>` reads only the regular, UTF-8 `SKILL.md` in the supplied directory. It checks the official frontmatter fields, YAML structure, name grammar and directory match, and documented size and parser limits. It does not inspect supporting files, modify content, make network requests, or execute scripts. A passing metadata check does not establish source authenticity or make any included scripts safe. See [the validation rules and limits](docs/registry.md#local-agent-skills-validation).

## CLI output and exit codes

Successful command output, including empty catalog and managed-installation states, goes to stdout and exits with code `0`. Clap usage errors, including missing required arguments or unknown options, go to stderr and exit with code `2`. Invalid lookup identifiers, unknown catalog IDs, corrupt or unsupported receipts, ownership conflicts, and command I/O failures go to stderr and exit with code `1`. The CLI does not yet provide JSON output or a broader categorized failure-code scheme.

Use `thrn --help` to see the commands currently available. Agent selection remains explicit. Cursor uses `.cursor/skills` and `~/.cursor/skills`; its native user directory participates in Cursor's cloud skill-sync behavior, while shared `.agents/skills` is not automatically synced to Cursor Cloud Agents ([Cursor docs](https://cursor.com/docs/skills), [customization help](https://cursor.com/help/customization/skills)). Gemini CLI uses `.gemini/skills` and `~/.gemini/skills`; its [skills documentation](https://geminicli.com/docs/cli/skills/) says `.agents/skills` takes precedence over `.gemini/skills` within the same discovery tier. Both agents can also discover `.agents/skills`, but THRN never treats discoverability as ownership by Cursor or Gemini. No npm or PyPI package is provided.

## Implementation notes

- [Architecture](docs/architecture.md)
- [CLI contract](docs/cli.md)
- [Security model](docs/security-model.md)
- [Installation receipts](docs/receipts.md)

## Codex GUI delivery and recovery

After implementing a task in the Codex GUI, run `bash scripts/finish-task.sh "type: description"` from the repository root. The script reruns verification before delivery, and the pre-push hook verifies the commit again.

If a branch commit was pushed but pull-request creation failed, run the same command again. With no new changes, the script verifies the checkout, confirms that the exact local commit is already on `origin`, then creates the draft PR or reuses the existing one. It does not make an empty commit or push a different commit. New changes must pass verification before a commit or push.

The CLI does not execute skill instructions. No npm or PyPI package is provided.
