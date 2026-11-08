# CLI contract

This page describes the executable that exists today. The CLI supports offline catalog discovery and installed-resource listing, explicit catalog refresh, local skill validation, `doctor`, Codex project installation planning and installation, and receipt-backed Codex project removal. Update, user scope, other agents, configuration, completions, JSON output, and noninteractive options are not implemented.

## Available commands

| Invocation | Behavior |
|---|---|
| `thrn --help` | Print root usage and the available command list. |
| `thrn --version` | Print the package name and version from Cargo metadata. |
| `thrn doctor` | Print version, reported OS and architecture, and the scope of checks performed. It does not perform filesystem or network checks. |
| `thrn doctor --help` | Print help for the `doctor` subcommand. |
| `thrn skills list` | List all catalog skills in canonical ID order, using a validated local cache or bundled metadata. Never uses the network. |
| `thrn skills list --installed` | List validated THRN-managed installation receipts. Missing or empty storage is a successful empty result; corrupt, unsupported, duplicated, conflicting, linked, or unknown content is an error. Shows recorded ownership and says current content integrity was not checked. Never uses the network or writes files. |
| `thrn skills search [query]` | Search the newest valid local catalog cache, or bundled metadata if no valid cache exists. Omit the query to list all entries. Never uses the network. |
| `thrn skills search --refresh [query]` | Fetch and validate the fixed trusted HTTPS catalog, atomically update the cache, and search the refreshed data. |
| `thrn skills info <id>` | Show curated metadata from the newest locally available validated catalog for an exact canonical ID or explicit alias. Never uses the network. |
| `thrn skills validate <path>` | Read and validate the `SKILL.md` in a local skill directory. |
| `thrn skills install <id> --agent codex --dry-run` | Fetch the exact pinned GitHub source, validate and stage the selected skill, inspect the destination, and print a complete read-only plan. |
| `thrn skills install <id> --agent codex` | Install the validated skill into the current project and record a durable THRN ownership receipt. No confirmation prompt is required. |
| `thrn skills install <id> --agent codex --project <path>` | Install into the selected existing project directory instead of the current directory. |
| `thrn skills install <id> --agent codex --project <path> --dry-run` | Print a read-only plan for the selected existing project directory. |
| `thrn skills remove <canonical-id> --agent codex` | Remove one fully verified receipt-backed skill from the current project. Does not use the catalog or network. |
| `thrn skills remove <canonical-id> --agent codex --project <path>` | Remove one fully verified receipt-backed skill from the selected existing project. |
| `thrn skills remove <canonical-id> --agent codex --dry-run` | Print the exact verified destination, receipt-listed files, counts, and bytes without changing the project or receipt store. |
| `thrn skills remove <canonical-id> --agent codex --project <path> --dry-run` | Print a read-only removal plan for the selected project. |

The root parser requires a subcommand. Clap also supplies its normal short-help and short-version aliases.

## Output and exit status

Successful help, version, doctor, list, search, info, validation, conflict-free dry-run, and completed installation output goes to stdout and exits `0`. Empty catalog and empty managed-installation states are successful. Catalog list, search, and info identify their selected catalog source (`local cache` or `bundled metadata`) and freshness. Installed listing reads validated receipts only and explicitly says current content integrity was not checked. Clap usage errors go to stderr and exit `2`. Invalid or unknown IDs, unsupported agents, network or source validation errors, unsafe or conflicting destinations, existing unmanaged skills, duplicate managed installs, active writer locks, recovery-required state, receipt or journal corruption, and output failures go to stderr and exit `1`. Failures after directory commitment identify the incomplete state and never print the full success message. Receipt publication followed by journal-cleanup failure is also an error even though the receipt may be visible. Recovery conflicts instruct the user to preserve the files and inspect the recorded operation; do not blindly delete receipts, journals, locks, or installed directories.
Successful removal follows the same stdout/exit-0 rule as installation. For removal, only an exact canonical resource ID recorded in a valid schema-v1 receipt is accepted; aliases and skill directory names are not resolved. The receipt must identify `agent: codex`, `scope: project`, and the selected canonical project root. THRN verifies the exact `<project>/.agents/skills/<skillName>` path, transaction identities, all receipt-listed files and directories, each file's byte count and SHA-256, and the absence of additional or linked objects. The catalog is not loaded, refreshed, or contacted. A missing, corrupt, or conflicting receipt; unmanaged directory; modified, missing, or extra content; identity change; lock contention; or unresolved transaction exits nonzero and preserves uncertain state. Once mutation begins, errors state that removal may be incomplete and never print the success block.

### Installation planning and installation

```sh
thrn skills install frontend --agent codex --dry-run
thrn skills install frontend --agent codex --project ./my-project --dry-run
thrn skills install frontend-design --agent codex
thrn skills install frontend-design --agent codex --project ./my-project
```

The catalog is loaded with `CatalogService` from the newest validated local cache or bundled
metadata. It is not refreshed by this command; use `skills search --refresh` separately. The command
then fetches the resource's full pinned GitHub commit, validates the archive and selected skill,
and stages its files privately. Both dry-run and installation can contact GitHub and download the
pinned archive. Installation does not automatically refresh the catalog.

M2-03 requires `--agent codex` and supports project scope only. The project defaults to the current
working directory; `--project` must name an existing directory. THRN resolves and displays that
project root. The destination is
`<project-root>/.agents/skills/<validated-skillName>`, following the [Codex build-skills
guide](https://learn.chatgpt.com/docs/build-skills). Catalog IDs and aliases are resolved through
validated metadata and are never used as the destination folder name.

Dry-run output shows the resource, skill name, pinned repository and revision, agent, scope, project
root, destination, directories, every regular file and its size, and total counts. It creates no
project files or receipt. Its output remains backward-compatible.

Without `--dry-run`, the command keeps the same validated staged content and immutable plan alive
through the M2-06 recovery-aware `commit_and_record` operation. The engine revalidates the
destination, rejects conflicts, commits without replacement, verifies installed hashes, publishes
the strict schema-v1 receipt, records journal completion, and returns a verified receipt. Only then
does the CLI print `Installed THRN skill`, the canonical resource ID, installed skill name, Codex
project scope, exact destination, pinned source revision, file and byte counts from that receipt,
and `Ownership receipt: recorded`.

The only supported target is explicit `--agent codex` at project scope. The default root is the
current working directory; `--project` must name another existing directory and cannot be a
symlink. The canonical destination is
`<project-root>/.agents/skills/<validated-skillName>`. The command resolves only a canonical
catalog ID or an explicit alias; it never guesses from a skill directory name. There is no prompt
after an explicit install command.

Existing managed installations and manually installed skills are rejected without overwriting
content. Files, symlinks/reparse points, unsupported objects, case-insensitive collisions, unsafe
roots, and unreadable paths also fail closed. Each inspected directory is bounded to 16,384
entries. The install source is not user-configurable: it comes from validated catalog metadata at
a full pinned GitHub commit and uses the existing GitHub/codeload origin, redirect, timeout, and
download-size restrictions.

A successful install requires directory commitment, durable receipt publication, journal
finalization, and cleanup to complete. A failure after directory commitment leaves the installed
directory for journal recovery and returns nonzero without a success message. A receipt can be
published before journal cleanup fails; that state is still reported as incomplete. The next
recovery-aware install or remove can resolve valid interrupted journal state, but there is no public
recovery command. Corrupt journals, uncertain ownership, modified partial content, or identity
conflicts need manual inspection. Preserve the directory, receipt, journal, quarantine, and
writer-lock state; never delete them blindly. `skills list --installed` remains offline and
read-only, and says that current content integrity is not checked.

### Receipt-backed removal

```sh
thrn skills remove uncharted --agent codex --dry-run
thrn skills remove uncharted --agent codex
thrn skills remove uncharted --agent codex --project ./my-project
```

Removal defaults to the current directory. `--project` must select an existing project directory.
`--agent codex` is required; only Codex project scope is implemented. The ID must be the exact
canonical ID in the ownership receipt. It does not accept an alias or infer an ID from the installed
folder, and it does not require the current catalog or network.

Dry-run verifies the receipt and whole current tree, then prints the recorded destination, each
file and size, total counts and bytes, and verification outcome. It does not create receipt or
journal storage, or change the project. Any conflict returns nonzero with no removal plan claiming
success.

An actual removal obtains the existing receipt-store writer lock, recovers or rejects previous
operations, rechecks the receipt and complete tree, and writes a separate versioned removal
journal. It renames the verified directory to a random `.thrn-removal-<operation-id>` sibling with
a no-replace operation. Cleanup deletes only exact receipt-listed regular files after identity,
link-count, byte-count and hash checks; it records a durable cleanup intent for each object first
and removes directories only when their recorded identities match and they are empty. The receipt
is retired only after the quarantined tree is completely removed. Empty `.agents` and
`.agents/skills` parents remain.

After a failure or process termination, install admission and installed listing fail closed while a
removal journal remains. A later install or remove operation acquires the same lock and runs
recovery. Recovery can retire a prepared operation or restore a verified original tree before
deletion begins; after deletion starts it resumes forward only when remaining objects and prior
cleanup intents prove ownership. Missing objects are accepted only with their exact cleanup intent.
Uncertainty preserves remaining project content, receipt, and journal; do not manually delete those
items. See [the removal recovery state machine](recovery.md#receipt-backed-removal-state-machine).

Successful output is printed only after journal completion and receipt retirement:

```text
Removed THRN skill
Resource: <canonical-id>
Skill name: <skillName>
Agent: codex
Scope: project
Destination: <canonical-project>/.agents/skills/<skillName>
Files removed: <count>
Bytes removed: <count>
Ownership receipt: retired
```

## Local skill validation

```sh
thrn skills validate ./path/to/my-skill
```

On success, THRN reports the validated name, description, optional license, compatibility and
allowed-tools values, and the number of additional metadata entries. For example:

```text
Skill validation passed
Name: valid-skill
Description: A minimal synthetic skill used to verify local metadata validation.
License: (not specified)
Compatibility: (not specified)
Allowed tools: (not specified)
Additional metadata entries: 0
```

Failures list actionable frontmatter or file errors on stderr. The validator reads only the regular
`SKILL.md`, has a 256 KiB file ceiling, and applies YAML complexity budgets documented in the
[registry and validation notes](registry.md#local-agent-skills-validation). It does not traverse,
modify, or execute supporting files.

## Platform considerations

`doctor` reports Rust's `std::env::consts::OS` and `ARCH` values. This pair is descriptive metadata, not a full Rust target triple. Doctor text is composed with LF newlines. Clap formats help and diagnostics; consumers and tests should check the meaning, status, and output channel rather than assume byte-for-byte identical help text across platforms or Clap versions.

## Offline discovery and catalog refresh

Default `skills list`, `skills search`, and `skills info` are offline. They try the validated cache first, then the embedded catalog. Cached data is fresh through 24 hours after its successful refresh; older cache remains usable and is labeled stale. A missing, corrupt, incompatible, or invalid cache falls back to bundled metadata. The fallback does not trigger a refresh. The cache lives in the OS-standard application cache directory: Linux uses `$XDG_CACHE_HOME/thrn` or `~/.cache/thrn`, macOS uses `~/Library/Caches/im.thrn.thrn`, and Windows uses `%LOCALAPPDATA%/thrn/thrn/cache`. Set `THRN_CACHE_DIR` to an alternate absolute directory when needed; it changes only the local cache path. The file is named `catalog-v1.cache`.

`skills list --installed` reads only `<application-data-directory>/receipts`, resolved by `directories::ProjectDirs::from("im", "thrn", "thrn")`. `THRN_DATA_DIR` can select an alternate absolute application-data root; THRN checks its `receipts` child and never creates it. Missing or empty storage reports that no THRN-managed installations are recorded. Valid schema-v1 receipts display the canonical resource ID, actual skill directory name, agent, scope, project destination, pinned revision, and an explicit `current content integrity: not checked` status. Unknown, corrupt, unsupported, duplicated, conflicting, linked, or nonregular receipt storage fails closed. Skills present in agent folders outside THRN are not checked. This command does not invoke catalog discovery and does not need a catalog cache. See [receipt design](receipts.md).

`thrn skills search --refresh [query]` is the catalog-refresh network path. It fetches only `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`. The request requires HTTPS certificate verification, follows no redirects, uses connection/body/overall timeouts, and reads no more than 1 MiB. The response must be UTF-8 and pass the catalog schema validator before it is persisted. A failed request, response validation, or cache commit exits nonzero on stderr. A failed commit leaves the earlier cache intact. The refresh command never fetches skill directories or executes catalog or skill content. `skills install --dry-run` separately fetches only the selected resource's pinned GitHub archive through the M2-02 source pipeline.

The endpoint is curated and operated by THRN. HTTPS protects the connection to that endpoint; neither the endpoint, catalog inclusion, nor a full Git commit SHA proves the identity or safety of an individual skill publisher. Process-level CLI tests use temporary cache roots and injected environment, and catalog service tests use fake transports, clocks, and cache directories. No test contacts the public endpoint or reads a real user cache. When a future command supports `--no-input`, tests must verify that ambiguous choices fail with an actionable error rather than selecting a target implicitly.
