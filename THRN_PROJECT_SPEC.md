# Thrn — Universal Developer CLI

> **Standalone project specification and implementation handoff**  
> **Status:** Planned; no implementation or publication claimed  
> **Research checked:** November 3, 2026  
> **Primary implementation language:** Rust  
> **Project name / executable:** `thrn`  
> **Proposed website:** `thrn.im`  
> **Proposed documentation:** `docs.thrn.im`  
> **First product/catalog:** `skills.thrn.im`

---

## 0. Executive decision

Build **Thrn** as a secure, small, universal developer CLI, **not** a skill-only installer. Its initial capability is discovering, installing, updating, and removing **Agent Skills** hosted in public GitHub repositories. Its internal design must allow other resource types (developer tools, reusable project templates, and integrations) without rewriting command parsing, resolution, fetching, integrity verification, installation records, or release tooling.

Use **one native Rust implementation**. Distribute the *same CLI behavior* through:

```bash
# npm — Node.js ecosystem (target; not published yet)
npx thrn skills add uncharted

# PyPI — Python/uv ecosystem (target; not published yet)
uvx thrn skills add uncharted

# Native installed executable (target; not published yet)
thrn skills add uncharted
```

**Important:** These are product targets, not working commands until the respective npm and PyPI names are secured and the packages are released. Owning `thrn.im` does **not** grant ownership of `thrn` on npm, PyPI, or crates.io. Verify and reserve names independently. If a name is unavailable, use an appropriate fallback and document the resulting invocation rather than pretending the original name works.

### Decision rationale

- **Rust:** strong compile-time type and ownership guarantees; safe Rust prevents data races; well suited to local filesystem operations and binary distribution. It does *not* automatically prevent zip-slip, malicious source material, application logic errors, compromised registries, or unsafe subprocess execution.
- **One core:** the CLI is the product. `npx`, `uvx`, and the native binary are distribution channels; they must not develop divergent command semantics.
- **Modular commands:** `thrn skills ...` now, `thrn tools ...` and `thrn templates ...` later. Do not create a universal replacement for npm/PyPI/Cargo.
- **Local-first:** v1 needs no authentication server, mandatory user accounts, database, analytics collection, or always-on backend.
- **Security and recoverability:** predictable local writes, validated content, explicit confirmation of destructive operations, reversible updates, truthful errors, and verification of downloaded data.

---

## 1. Product definition

### 1.1 Mission

Make reusable developer resources easy to **discover, install, manage, and trust** across operating systems and ecosystems through one branded command-line interface.

### 1.2 Users

1. Developers installing skills for Claude Code, Codex, Cursor, Gemini CLI, or other compatible agents.
2. Maintainers publishing skills as ordinary files in public GitHub repositories.
3. Future users of Thrn-managed standalone tools and project templates.

### 1.3 Example experience

```bash
thrn --version
thrn --help
thrn skills search brainstorm
thrn skills info uncharted
thrn skills add uncharted --agent codex
thrn skills list
thrn skills check
thrn skills update uncharted
thrn skills remove uncharted
thrn doctor
```

`uncharted` is a **registry-facing alias** for a skill whose actual standards-compliant directory and `SKILL.md` name may be `uncharted-ideation`. The canonical skill name remains `uncharted-ideation`; aliases must be resolved explicitly by metadata, never by guessing.

### 1.4 In scope for public v1

- A real, cross-platform `thrn` executable.
- Discover skills from a small maintained HTTPS JSON registry, initially committed to the source repository and hosted as static content.
- Fetch full skill directories from **public GitHub repositories** at an immutable commit SHA.
- Validate Agent Skills metadata and file paths.
- Install skill directories at **project** or **user/global** scope, for at least Codex, Claude Code, Cursor, and Gemini CLI.
- Add, search/list, info, check, update, and remove skills, with a durable installation receipt.
- Offline-safe listing of installed resources.
- `--json`, `--no-input`, `--verbose`, `--quiet`, useful exit codes, and shell completions where appropriate.
- Native releases and npm/PyPI distribution that execute the same Rust program.
- Tests, security documentation, MIT/Apache-2.0 (or another consciously chosen) licensing, and clear README/docs.

### 1.5 Explicit non-goals for v1

- Hosting arbitrary private Git repositories or managing GitHub tokens.
- Running arbitrary installation scripts from downloaded skills.
- Unrestricted third-party publishing, discovery ranking, or a user-generated public marketplace.
- A full package-manager dependency solver or replacement for npm/uv/Cargo.
- Automatic modification of source-controlled project files without clearly describing the changes.
- Executing code from templates or tools as a side effect of installing a skill.
- A GUI/web dashboard that must exist before the CLI is usable.
- ChatGPT Web skill installation: a local CLI cannot directly install capabilities into ChatGPT's hosted interface.

---

## 2. Naming and brand architecture

| Surface | Proposed name | Purpose |
|---|---|---|
| Native executable | `thrn` | One Rust CLI |
| npm package | `thrn` | Allows `npx thrn ...` when available |
| PyPI package | `thrn` | Allows `uvx thrn ...` when available |
| Cargo crate | `thrn` | Optional source installation, if available |
| Main site | `thrn.im` | Umbrella brand |
| Docs | `docs.thrn.im` | CLI and developer documentation |
| Skills catalog | `skills.thrn.im` | Skills browsing, search, install instructions |
| Registry | Static JSON on a trusted HTTPS endpoint | Source metadata; no API server necessary in v1 |

**Naming check before release:** Inspect availability and trademark conflicts for each package/brand independently. If npm and PyPI naming diverge, document the exact command; with `uvx --from PACKAGE COMMAND ...`, the package name may differ from the exposed CLI name.

### Architecture principles

- **Universality at the core, focus at the feature level.** Build only the skills module initially.
- **Avoid premature plugin engines.** Use Rust traits and a resource-type enum to isolate responsibilities; no dynamic loading of arbitrary executables in v1.
- **Typed internal models.** Validate external metadata at boundaries; distinguish `SkillId`, `SkillName`, `AgentId`, `CommitSha`, `ResourceType`, and `InstallScope` rather than using unvalidated strings everywhere.
- **SemVer for the CLI.** Registry schema versions and skill versions are independent of CLI releases.
- **No silent network behavior beyond requested operations.** `thrn --help`, `thrn --version`, and `thrn skills list --installed` must work offline.

---

## 3. CLI command contract

This is the proposed stable v1 interface. Implement or document adjustments before release; do not advertise unimplemented commands.

### 3.1 Root commands

```text
thrn --help
thrn --version
thrn doctor
thrn config show
thrn completions <bash|zsh|fish|powershell>
```

Possible future namespaces: `thrn tools ...`, `thrn templates ...`, `thrn registry ...`.

### 3.2 Skill commands

| Command | Behavior |
|---|---|
| `thrn skills search [query]` | Search registry cache or retrieve catalog; clearly indicate offline/stale results |
| `thrn skills info <id>` | Show description, source, version, required files, supported agent targets, trust information |
| `thrn skills add <id>` | Resolve source, validate, show plan, install into a selected target |
| `thrn skills list` | List known skills in catalog; explicit source indicator |
| `thrn skills list --installed` | List installed skills from receipts without network |
| `thrn skills check` | Check for available updates, without mutating files |
| `thrn skills update [id]` | Re-verify and atomically replace Thrn-managed installs |
| `thrn skills remove <id>` | Remove only recorded Thrn-managed files/links; avoid deleting user changes |
| `thrn skills validate <path>` | Validate a local skill directory against the standard |

Suggested `add` options:

```bash
thrn skills add uncharted --agent codex --scope project
thrn skills add uncharted --agent claude-code --scope user
thrn skills add uncharted --agent cursor --agent gemini --scope project
thrn skills add uncharted --all-detected --scope project
thrn skills add uncharted --agent codex --dry-run
thrn skills add uncharted --agent codex --json --no-input
```

**Defaults:** `--scope project`; detect available agents and prompt interactively if more than one match. If detection is ambiguous under `--no-input`, **fail with an actionable error** instead of guessing. An explicitly selected agent may create its configured target directory. Do not auto-install into every possible agent without consent.

Use `--force` only for narrowly defined conflicts, not as a blanket permission to delete unrelated files. Always show the complete mutation plan in `--dry-run` mode.

### 3.3 Output and exit semantics

- Human-readable, concise default output; structured JSON when `--json` is selected.
- JSON mode writes one valid JSON document to stdout; diagnostics go to stderr; no spinner/ANSI color in JSON.
- Success exit code `0`; meaningful nonzero codes for usage/validation, network, conflict, permission, and integrity failures.
- Respect `NO_COLOR`, `CI`, `--no-input`, and standard terminal detection.
- Never print bearer tokens, credentials, or private local path details in verbose logs unnecessarily.

---

## 4. Agent Skills standard and compatibility

### 4.1 Required skill layout

The [official Agent Skills specification](https://agentskills.io/specification) defines a skill directory with a `SKILL.md` file containing frontmatter. Other files may include `references/`, `scripts/`, or `assets/`.

```text
uncharted-ideation/
├── SKILL.md
├── references/
│   ├── methodology.md
│   └── evaluation.md
└── assets/
    └── templates.md
```

Example `SKILL.md`:

```markdown
---
name: uncharted-ideation
description: Discover unconventional, defensible, and testable ideas through structured exploration and evaluation. Use for startup, research, engineering, and product ideation.
---

# Uncharted Ideation

Follow the instructions in this skill. Load additional reference files only as needed.
```

Validate at least: `name` present, 1–64 characters, lowercase ASCII letters/digits/hyphens, no leading/trailing/doubled hyphens, matching parent folder; `description` present and 1–1024 characters; valid YAML frontmatter. Preserve optional standard fields and all legitimate supporting files. Do not replace the standard with a Thrn-specific skill format. For independent validation, see the official `skills-ref` reference implementation.

### 4.2 Verified target directory conventions (as researched)

| Agent | Project scope | User scope | Notes |
|---|---|---|---|
| OpenAI Codex | `.agents/skills/<name>/` | `~/.agents/skills/<name>/` | Universal Agent Skills directory |
| Claude Code | `.claude/skills/<name>/` | `~/.claude/skills/<name>/` | Must handle Claude-specific path |
| Cursor | `.agents/skills/<name>/` or `.cursor/skills/<name>/` | `~/.agents/skills/<name>/` or `~/.cursor/skills/<name>/` | Shared path recognized; avoid unnecessary duplicate copies |
| Gemini CLI | `.agents/skills/<name>/` or `.gemini/skills/<name>/` | `~/.agents/skills/<name>/` or `~/.gemini/skills/<name>/` | Shared alias recognized |

**Sources:** [OpenAI CLI installation](https://developers.openai.com/api/reference/cli), [Cursor Skills](https://prod.cursor.com/docs/skills), [Gemini CLI Skills](https://geminicli.com/docs/cli/skills/), [Vercel Skills CLI](https://github.com/vercel-labs/skills/blob/main/docs/commands.md).

**Implementation decision:** For v1, default to **copying** validated content to each required destination, because copying is predictable across Windows/macOS/Linux and does not require elevated symlink privileges. Each copied install gets a receipt and an identical source revision. Optionally design a future content-addressed shared directory plus symlink mode, but do not make symlinks a prerequisite. Ensure `.agents/skills` is not redundantly populated if the selected agents already recognize the shared directory.

Do not equate installation in local Codex/Claude/Cursor/Gemini with installation in ChatGPT Web. Target agents are distinct products and may change discovery semantics over time: verify path rules before each compatibility release.

---

## 5. Recommended Rust technology stack

Use **current mutually compatible, maintained releases**, captured precisely in `Cargo.lock`. Avoid blindly pinning package versions in this specification, because they can become stale.

| Concern | Recommended technology | Why |
|---|---|---|
| CLI parsing | `clap` (derive) | Typed subcommands, help, completions |
| Core data structures | Rust standard library | Minimize unnecessary dependencies |
| JSON | `serde`, `serde_json` | Typed external metadata |
| YAML | `serde_yaml` alternative under active maintenance, or `serde_yml` after security review | Parse SKILL.md frontmatter; **evaluate crate maintenance before selection** |
| HTTP | `reqwest` with rustls TLS | HTTP clients, timeouts, redirects |
| Async runtime | `tokio` only if needed | Efficient I/O; simple v1 could use blocking HTTP instead |
| Errors | `thiserror` (typed domain), `anyhow` (CLI boundary) | Clear recoverable failures |
| Temporary directories | `tempfile` | Staged downloads and installations |
| Hashing | `sha2` | Verify SHA-256 hashes |
| TOML / config | `toml`, `serde` | Local configuration |
| Directory discovery | `dirs` or `directories` | OS-specific user cache/config/data locations |
| Archives | `zip`, `tar`, `flate2` when required | Safely handle vetted GitHub archives |
| Diagnostics | `tracing` optional | Useful debug logs without excessive framework code |
| Tests | `cargo test`, `assert_cmd`, `predicates`, `tempfile` | Integration testing and isolated fixtures |
| Static checks | `cargo fmt`, `cargo clippy`, `cargo deny`, `cargo audit` | Quality and dependency/security checks |
| Native/npm releases | [`dist` (formerly `cargo-dist`)](https://axodotdev.github.io/cargo-dist/) | Platform builds, GitHub releases, npm installer |
| PyPI binaries | [`maturin` with `bin` bindings](https://www.maturin.rs/bindings) | Put compiled Rust executable directly into platform wheels |
| CI | GitHub Actions | Multi-OS build, test, and publish |

**Use `std::path::PathBuf` for OS filesystem paths.** Avoid assuming every valid path is UTF-8 or that a path remains safe merely because its string representation appears normalized.

**Do not introduce PyO3** just for PyPI distribution: Maturin's `bin` mode can package a plain Rust binary as a Python-distributed executable. No Python implementation of Thrn is needed.

---

## 6. Proposed repository architecture

Prefer a **single Rust binary crate** initially; extract a workspace only when there is a demonstrated need for separate publishable crates. This reduces complexity for both `dist` and Maturin.

```text
thrn/
├── Cargo.toml                  # Rust crate + binary metadata
├── Cargo.lock                  # Commit for reproducible builds
├── rust-toolchain.toml         # Pinned stable toolchain
├── pyproject.toml             # Maturin PyPI binary wheel metadata
├── README.md
├── LICENSE
├── CHANGELOG.md
├── SECURITY.md
├── CONTRIBUTING.md
├── AGENTS.md                    # Repository-specific coding agent instructions
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── skills.rs
│   │   ├── doctor.rs
│   │   └── config.rs
│   ├── models/
│   │   ├── mod.rs
│   │   ├── resource.rs
│   │   ├── registry.rs
│   │   └── receipt.rs
│   ├── registry/
│   │   ├── mod.rs
│   │   ├── fetch.rs
│   │   └── resolve.rs
│   ├── sources/
│   │   ├── mod.rs
│   │   └── github.rs
│   ├── install/
│   │   ├── mod.rs
│   │   ├── plan.rs
│   │   ├── transaction.rs
│   │   ├── security.rs
│   │   └── receipts.rs
│   ├── agents/
│   │   ├── mod.rs
│   │   ├── codex.rs
│   │   ├── claude.rs
│   │   ├── cursor.rs
│   │   └── gemini.rs
│   └── skills/
│       ├── mod.rs
│       └── validate.rs
├── registry/
│   └── catalog.v1.json        # Initial curated catalog (static)
├── fixtures/
│   ├── good-skill/
│   ├── bad-frontmatter/
│   ├── malicious-traversal/
│   └── collision/
├── tests/
│   ├── cli_smoke.rs
│   ├── skill_validation.rs
│   ├── install_project.rs
│   ├── install_user.rs
│   ├── updates.rs
│   └── security.rs
├── docs/
│   ├── architecture.md
│   ├── cli.md
│   ├── registry.md
│   ├── security-model.md
│   └── publishing.md
├── .github/
│   └── workflows/
│       ├── ci.yml
│       └── release.yml
└── website/                    # Optional later Next.js docs/catalog app
```

**Separate repositories** may host actual skills (e.g., `YOUR_GITHUB_OWNER/skills`) and future templates/tools. This repository manages their installation, not their authoring lifecycle. Avoid putting large skill/reference assets into the CLI binary.

### Module responsibilities

- `cli`: Parse and dispatch commands; do not embed installer internals.
- `models`: Typed resource metadata, source identity, aliases, and installation receipts.
- `registry`: Fetch, cache, parse, version-check, and resolve curated resource metadata.
- `sources`: Resolve and fetch approved source types; **GitHub public repositories only** for v1.
- `skills`: Validate standards-compliant skill structures.
- `agents`: Define destinations and agent detection behavior; no network access.
- `install`: Produce immutable install plans, stage files, verify them, commit/rollback, and own receipts.
- `doctor`: Report local compatibility/permissions and installation health without mutation.

Internal traits may resemble `ResourceSource`, `ResourceValidator`, `TargetAdapter`, and `ResourceInstaller`. Keep boundaries simple; do not build a general-purpose dynamic extension runtime prematurely.

---

## 7. Source registry and version model

### 7.1 v1 catalog

A committed, versioned JSON catalog is sufficient for launch. It should be hosted via static HTTPS (e.g. a pinned GitHub Pages endpoint) and cached locally. The sample below is **illustrative**; replace owner, repository, commit, and integrity data with real verified values.

```json
{
  "schemaVersion": 1,
  "resources": [
    {
      "id": "uncharted",
      "kind": "skill",
      "displayName": "Uncharted",
      "skillName": "uncharted-ideation",
      "description": "Systematic unconventional ideation and evaluation.",
      "version": "1.0.0",
      "source": {
        "provider": "github",
        "owner": "YOUR_GITHUB_OWNER",
        "repository": "skills",
        "path": "skills/uncharted-ideation",
        "commit": "FULL_40_CHARACTER_GIT_COMMIT_SHA"
      }
    }
  ]
}
```

Validation must reject placeholders and malformed commit hashes in a real release. Names should be unambiguous; no two published resource IDs or aliases may resolve to different artifacts without explicit namespace qualification.

### 7.2 Security and integrity for registry data

- Resolve a mutable Git ref (if ever allowed) to a concrete commit **before** building the install plan. Prefer immutable pinned commit references in the curated registry.
- Prefer a curated **trusted HTTPS registry** initially; do not imply TLS alone provides publisher-level content authenticity against a compromised registry.
- Download only the specified directory from the specified commit; stage it in a temporary directory.
- Enforce **uncompressed bytes, compressed bytes, total file count, file size, and path-depth limits**.
- Hash all content canonically and save a receipt. For stronger supply-chain security, add a *signed* manifest/attestation in a later milestone; a plain checksum served by the same compromised endpoint does not authenticate the publisher.
- Reject archive path traversal, absolute paths, Windows drive prefixes, alternate-data-stream names, symlink/hardlink escapes, unsupported file types, duplicate and case-fold-colliding destinations, and links into external paths.
- Use explicit network timeouts and response-size limits. Prevent untrusted host redirects; enforce an allowlist for GitHub endpoints in v1. Account for proxy environments without leaking tokens.
- Verify license and content provenance in the CLI's `info` view. Treat downloaded skill content as **untrusted instructions and files**.

Reference: ZIP's `ZipFile::enclosed_name()` exists specifically because archive names can escape extraction roots; see [zip documentation](https://docs.rs/zip/latest/zip/read/struct.ZipFile.html).

### 7.3 Future extensibility

A future registry could support `kind: tool`, `kind: template`, and language-specific `kind: integration`, each with a **resource-type handler**. Do not promise automatic library installation: language libraries should generally be consumed with their native package managers, while Thrn can provide discovery/scaffolding/orchestration.

Do not deploy a database or arbitrary third-party registry feature until justified by actual use cases.

---

## 8. Installer transaction model (most important engineering component)

The installer must prefer **failing safely** over succeeding after silently corrupting a user's project.

### 8.1 Install operation

1. Parse CLI options; identify requested skill and intended agents/scope.
2. Load trusted catalog (cached or network), validate schema and resolve exact source commit.
3. Compute all target directories; verify permissions, boundaries, and conflicts.
4. Show intended writes and ask for consent where necessary; honor `--dry-run`.
5. Fetch content to a bounded temporary location; verify origin, size, path types, metadata, and content hashes.
6. Validate `SKILL.md` and preserve optional supporting files.
7. Stage complete destination trees on the **same filesystem** as each target where possible.
8. Make backups of any existing *Thrn-owned* installations being updated; never overwrite user-owned content silently.
9. Commit using atomic rename where supported. Handle multiple target destinations as a **recoverable series of operations**, not as a falsely claimed single atomic transaction.
10. Write durable receipts only after successful commits. On any failure, attempt rollback, retain recovery information if rollback is incomplete, and report exactly what changed.

### 8.2 Ownership and receipts

Maintain Thrn-specific installation receipts in the platform-appropriate application-data directory, not inside arbitrary foreign application state. A receipt should record:

```json
{
  "schemaVersion": 1,
  "resourceId": "uncharted",
  "resourceKind": "skill",
  "skillName": "uncharted-ideation",
  "version": "1.0.0",
  "sourceCommit": "FULL_40_CHARACTER_GIT_COMMIT_SHA",
  "scope": "project",
  "projectRoot": "ABSOLUTE_PROJECT_PATH",
  "targets": [
    {
      "agent": "codex",
      "path": "ABSOLUTE_INSTALLED_DIRECTORY",
      "contentDigest": "sha256:COMPUTED_CONTENT_DIGEST"
    }
  ]
}
```

The JSON above is a **design example**, not a real digest. Choose and document a deterministic digest format, receipt identity, and schema migration policy before release. Receipts should allow multiple installations of the same skill in different projects/agents; the key is **not merely the skill name**.

Update/remove behavior:

- Refuse to clobber modified/unrecognized files by default.
- Provide `--dry-run` and a clear conflict report.
- On remove, delete only paths explicitly recorded and still owned by Thrn; refuse to recursively delete project directories not known to be owned.
- On updates, record old/new source versions and handle partial failures with recoverable backups.
- If a skill exists in a known destination but no Thrn receipt exists, consider it *externally managed* and require a deliberate import/overwrite decision.

### 8.3 Security acceptance cases

Tests must cover at least these hostile or failure scenarios:

- `../../` / absolute-path archive entries; `C:\` or UNC paths on Windows.
- Symlink pointing outside destination; symlink cycles; file/directory name collisions.
- Case-insensitive name collisions on Windows and common macOS volumes.
- Malformed YAML, oversized `SKILL.md`, missing required frontmatter, corrupt archives.
- Network disconnect, connection timeout, changed/invalid source revision, HTTP redirect to untrusted host.
- Unwritable destination, read-only filesystem, partially completed install, interrupted update, and rollback failure.
- Existing manually installed skill, duplicate installation, modified installed content, and `--force` collisions.
- Concurrent installation/update operations for the same project/skill; prevent conflicting writes via locking.
- Offline operations, incorrect JSON mode output, and non-interactive selection without enough information.

A successful happy-path test alone is not sufficient.

---

## 9. Local state, configuration, and privacy

- Config file: OS-appropriate user config directory (e.g., `~/.config/thrn/config.toml` on Linux where conventions permit).
- Cache: OS-appropriate cache directory; catalog and verified downloaded content may be cached.
- Receipts: OS-appropriate data/state directory, keyed by installation target and project identity.
- Prefer no telemetry by default; if telemetry is ever introduced, make it opt-in and clearly documented.
- Never require a cloud account for installing public skills.
- Do not install to global scope unless the user specifies or accepts it.
- `thrn doctor` reports relevant paths, permissions, supported agents, and registry connectivity, without changing files or attempting privileged operations.

Example future configuration shape:

```toml
[registry]
url = "https://YOUR_VERIFIED_REGISTRY_HOST/catalog.v1.json"
cache_ttl_seconds = 3600

[install]
default_scope = "project"
confirm_overwrite = true

[output]
color = "auto"
```

Avoid silently changing trust boundaries based on arbitrary environment variables. Document supported overrides and their security implications.

---

## 10. Cross-platform support

### Initial release matrix

| Target triple | Platform | Release tier |
|---|---|---|
| `aarch64-apple-darwin` | macOS Apple Silicon | Tier 1 |
| `x86_64-apple-darwin` | macOS Intel | Tier 1 |
| `x86_64-pc-windows-msvc` | Windows x64 | Tier 1 |
| `x86_64-unknown-linux-gnu` | Linux x64 (glibc) | Tier 1 |
| `aarch64-unknown-linux-gnu` | Linux ARM64 | Tier 1, if CI and wheel tags are validated |
| `aarch64-pc-windows-msvc` | Windows ARM64 | Later/experimental until tested |
| musl targets | Portable Linux variants | Later/optional |

Do not claim that a target is supported merely because Rust can theoretically cross-compile it. Every public target needs a built artifact, installation smoke test, and documented minimum environment (e.g., glibc requirements).

### Native CLI

Distribute `.tar.gz`/`.tar.xz` or `.zip` archives on GitHub Releases, with SHA-256 checksums and, where feasible, release provenance attestations. Use `dist` to generate release workflows/installers; manually review its output before enabling publishing.

### npm / `npx`

Use `dist`'s supported **npm installer** generation instead of writing a wholly separate TypeScript installer. It packages/locates and executes a prebuilt `thrn` native binary. The npm `bin` entry should expose the command `thrn`. Verify that `npx thrn --version` resolves the intended binary across supported environments. If package-name ownership prevents `npx thrn`, use the actual published package name and document its command.

### PyPI / `uvx`

Use **Maturin's binary (`bin`) bindings** to publish platform-specific Python wheels containing the same Rust executable. No Python translation or PyO3 binding is necessary. Ensure every built wheel has the correct executable location, platform tag, and command name; test `uvx thrn --version` on clean systems with supported OS and Python. If `uvx thrn` cannot resolve a platform wheel or package name, fail clearly; don't silently attempt compilation unless intentionally supported.

A minimal conceptual `pyproject.toml` for the *single-crate root* is:

```toml
[build-system]
requires = ["maturin>=1,<2"]
build-backend = "maturin"

[project]
name = "thrn"
version = "0.1.0"
description = "Universal developer CLI"
requires-python = ">=3.9"

[tool.maturin]
bindings = "bin"
```

**Before using this configuration**, confirm the Maturin version accepts the selected binary layout, package metadata, and wheel requirements. Keep `Cargo.toml` and PyPI package versions aligned through one release process. Maturin documents its `bin` mode explicitly: <https://www.maturin.rs/bindings>.

### Optional fourth distribution

`cargo install thrn --locked` can be supported if the `thrn` crate name is available and the crate is published to crates.io. This is *not* a prerequisite for the three primary distribution methods.

---

## 11. Build and release engineering

### 11.1 Local development prerequisites

- Rust toolchain via rustup (stable; pin the toolchain in the repo).
- Git and Cargo.
- Node.js/npm for npm smoke testing.
- `uv`/Python and Maturin for Python binary wheel testing.
- `dist` tool for native release planning.
- GitHub repository and release permissions for public distribution.

**Starter commands** (after creating the repository; modify to match the final crate layout):

```bash
cargo new thrn --bin
cd thrn
cargo add clap --features derive
cargo add serde --features derive
cargo add serde_json reqwest thiserror anyhow tempfile sha2
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --help
cargo run -- --version
```

Choose compatible HTTP/TLS and (if necessary) async features after testing the chosen versions; don't assume this minimal dependency list constitutes a finished downloader.

Install `dist` using a current documented method, then use its setup flow:

```bash
dist init
dist plan
```

Consult the current [dist documentation](https://axodotdev.github.io/cargo-dist/book/workspaces/simple-guide.html); `dist init` may regenerate release workflows, so keep generated CI under version control and avoid hand edits that will be unexpectedly overwritten.

### 11.2 CI on each pull request

- Format check: `cargo fmt --all -- --check`.
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`.
- Unit + integration tests: `cargo test --all-targets`.
- Dependency advisory/license/policy checks (`cargo deny` / `cargo audit`, correctly configured).
- Build and smoke tests on Linux, macOS, and Windows.
- Offline tests with network disabled and fixture-backed source/registry responses.
- Archive fuzz/property-based tests as a follow-up hardening task.
- Keep generated artifacts out of the Git repository unless intentionally committed.

### 11.3 Release pipeline

Use a protected `vX.Y.Z` tag and GitHub Actions:

1. Verify tag, crate version, and PyPI/npm release version all match.
2. Run full tests and security checks.
3. Build reproducible native artifacts for every supported target.
4. Generate checksums and, where available, GitHub artifact attestations.
5. Create and verify the GitHub Release.
6. Build the npm installer pinned to this **exact release**; verify the npm tarball before publishing.
7. Build platform-specific PyPI binary wheels using Maturin; verify each wheel in a fresh environment.
8. Publish npm and PyPI with **trusted publishing (OIDC)** when configured and supported, not long-lived registry tokens.
9. Run published-package end-to-end smoke tests before calling the release complete.
10. Publish release notes and update the docs/install commands.

For npm trusted publishing, note that current docs require npm CLI **11.5.1+** and Node **22.14.0+** for the publishing workflow; these are *release-tool prerequisites*, not necessarily the minimum Node version users need for the runtime npm package. See [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/).

Sources: [GitHub artifact attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations), [npm publishing](https://docs.npmjs.com/creating-and-publishing-unscoped-public-packages/), [uvx tool execution](https://docs.astral.sh/uv/guides/tools/), [dist](https://axodotdev.github.io/cargo-dist/).

### 11.4 Supply-chain precautions

- Pin GitHub Actions dependencies to reviewed commits; use least-privilege workflow permissions.
- Protect release tags and require approval for publishing.
- Use GitHub-hosted runners for registry OIDC where required.
- Do not run publisher workflows on untrusted pull-request code with publishing permissions.
- Prefer checksums, release attestations, and pinned inputs over unverified install scripts.
- Do not include credentials in release archives, generated npm packages, PyPI wheels, or logs.

---

## 12. Documentation and website

**CLI functionality comes first.** Do not block the CLI on a website.

A later Next.js website can include:

1. Home at `thrn.im`: umbrella project and product navigation.
2. Docs at `docs.thrn.im`: installation methods, command reference, supported agents, security, troubleshooting.
3. Skill catalog at `skills.thrn.im`: each skill's description, source repository, version, trust/provenance, compatibility, and copyable install commands.

Build pages from the **same registry metadata** used by the CLI. Avoid maintaining a second manually curated list of published skills. Start with static generation; add dynamic infrastructure only if search scale, submissions, or authenticated features require it.

Recommended docs sections: Quickstart, Commands, Agents & Scopes, Skill Standard, Security, Troubleshooting, Publishing Skills, Contributing, Changelog.

---

## 13. Implementation milestones and acceptance criteria

### M0 — Rust foundation

**Build:** Cargo project, clap CLI, typed errors, `--help`, `--version`, `doctor` skeleton, formatting/linting/test CI.

**Done when:** clean checkout builds on the primary developer OS; `cargo test`, `cargo clippy`, and `cargo fmt --check` pass; CLI help and version work offline.

### M1 — Standards-aware registry discovery

**Build:** versioned catalog schema, local test registry, cache, search, info, skill YAML validator, exact alias mapping, fixture tests.

**Done when:** registry IDs resolve to pinned commits; malformed catalog entries and invalid skills produce clear errors; offline `list --installed` and cache behavior are tested.

### M2 — Safe skill installations

**Build:** GitHub fetcher, size limits, path validation, per-agent destination adapters, install plans, staged writes, receipts, conflict detection, dry run, rollback.

**Done when:** a complete multi-file skill installs locally for at least one agent; the entire intended directory is present; malicious fixtures are rejected; failed installs do not leave falsely successful receipts.

### M3 — Full skill lifecycle and compatibility

**Build:** add/list/remove/check/update/validate; Codex, Claude Code, Cursor, Gemini target tests; global/project scope; `--json` and `--no-input`; concurrency control.

**Done when:** each agent/path combination passes a clean install, update, conflict, and remove test on supported OS configurations; human-modified files are preserved unless explicitly authorized.

### M4 — Native distribution

**Build:** `dist`, checksums, release artifacts, CI per target, end-to-end native smoke tests.

**Done when:** a clean supported machine can download the correct archive, verify integrity, run `thrn --version`, install a fixture skill, and uninstall it safely.

### M5 — npm and PyPI

**Build:** npm package via dist; PyPI wheels via Maturin `bin`; OIDC publication; package metadata; version consistency checks.

**Done when:** **all three** work from clean environments:

```bash
npx thrn --version
uvx thrn --version
thrn --version
```

And these yield equivalent resource behavior on the same OS and test fixtures:

```bash
npx thrn skills add uncharted --agent codex --scope project
uvx thrn skills add uncharted --agent codex --scope project
thrn skills add uncharted --agent codex --scope project
```

Use separate fresh sandboxes for tests to prevent mutual conflicts. Verify actual published package artifacts, not merely local development wrappers.

### M6 — Documentation and public launch

**Build:** finalized README, changelog, security policy, troubleshooting, simple website/catalog, release notes, public smoke tests.

**Done when:** a user unfamiliar with the repo can follow exactly one copyable command for their ecosystem, install a skill successfully, see where it was installed, check for updates, and remove it safely.

### Future milestones (not v1 requirements)

- Private GitHub repositories and credential helper integrations.
- Installation of other developer resources (`tools`), with a separate explicit execution permission model.
- `templates create` with variable interpolation and overwrite protections.
- Signed registry metadata / publisher attestations.
- First-class package-source plugins, trusted mirrors, community submissions, or registry search service.
- Homebrew, Winget, Scoop, crates.io, and other package-manager channels.
- Shell plugin completions, optional self-update mechanism with signature verification.

---

## 14. Implementation policy for coding agents

A development agent working on Thrn must follow these rules:

1. Read this document and inspect the **actual repository** before editing; don't invent existing files or tests.
2. Treat this document as product intent; adapt commands/dependencies only when current official documentation or tests justify a change.
3. Deliver working vertical slices (CLI → registry → validator → installer → tests), not large untested scaffolding.
4. Keep one native Rust implementation; never independently reimplement installation behavior in JavaScript or Python.
5. Make filesystem and network behavior explicit, secure, and testable.
6. Avoid `unsafe` Rust unless essential, precisely isolated, reviewed, and justified; avoid `unwrap()`/`expect()` on external input paths.
7. Never execute downloaded content implicitly.
8. Preserve existing user files and show conflicts; no indiscriminate recursive deletion.
9. Run format, lint, tests, and build for each meaningful change; fix failures or identify precisely what prevents them.
10. Include exact reproduction commands and status for completed, incomplete, and blocked acceptance criteria.
11. Do not claim npm/PyPI/Cargo name ownership, domain setup, or successful publishing without verifying it.
12. Do not prematurely add a backend or website as a prerequisite for core CLI operations.

---

## 15. Ready-to-paste prompt for a fresh development chat

> Attach **`THRN_PROJECT_SPEC.md`** to a new chat and paste the following prompt.

```text
Act as the lead Rust systems engineer, security engineer, and release engineer for Thrn.

Read the attached THRN_PROJECT_SPEC.md completely and treat it as the authoritative project specification. This is a fresh project; do not assume any implementation exists unless you inspect the repository.

Goal: build Thrn, a standalone, universal, cross-platform Rust developer CLI. The first working feature is secure installation and management of Agent Skills from public, pinned GitHub sources. The same Rust executable must ultimately be released through native archives, npm (`npx thrn`), and PyPI (`uvx thrn`). The architecture must support future tools and templates without becoming a generic package manager in v1.

Start by:
1. Inspecting the workspace and all existing files (if any).
2. Verifying current official documentation for the concrete Rust crates, Agent Skills standard, dist npm installer, Maturin bin wheels, and the intended agents' skill directories.
3. Creating a concise implementation plan based on milestones M0–M6, with dependencies, acceptance tests, risks, and decisions that need verification.
4. Implementing M0 as a genuine, working Rust CLI, not a mock or pseudocode skeleton.
5. Running cargo fmt, cargo clippy, cargo test, and a real CLI build; fixing issues before moving on.
6. Continuing incrementally through the milestones, implementing and verifying as much as the available environment permits.

Keep the design secure by default: bounded downloads, immutable source revisions, strict path checks, no implicit execution of downloaded files, atomic/recoverable installs, explicit receipt ownership, non-destructive removal, and reliable error handling.

Use one native Rust implementation. npm and PyPI are distribution layers, not independent product implementations. Do not publish packages, change DNS, push to remote, or make irreversible external changes without explicit approval. If a step is blocked by missing credentials, package ownership, a remote repository, or unsupported infrastructure, do all local work possible and clearly identify the precise remaining action.

At each milestone, report implemented files, verified tests, open risks, and the next dependency. Avoid placeholders and claims of verification without actually running the checks.
```

---

## 16. Open questions to verify before committing public interfaces

These are intentionally called out because assumptions here could break the release:

- **Package ownership:** Are `thrn` on npm, PyPI, and crates.io available and legally safe to publish? Check each; choose alternatives if necessary.
- **Public GitHub location:** What exact organization/repository will host the CLI and the separate skills collection?
- **Initial skill content:** What is the concrete `uncharted-ideation` commit containing valid `SKILL.md` and support files?
- **Release matrix:** Can Linux ARM64 wheels and macOS binaries be smoke-tested on real/representative environments? Mark any unverified targets experimental.
- **Agent behavior:** Have supported versions of Claude Code, Codex, Cursor, and Gemini been tested with the directory destinations and project scopes proposed here?
- **Registry authenticity:** For v1, what trusted static HTTPS URL serves the curated catalog, and how are updates governed?
- **License:** Is the CLI open-source under MIT/Apache-2.0 or another license, and are published skills individually licensed appropriately?
- **Website scope:** Is the first release CLI/docs-only or does it also include the simple static catalog?

None of these blocks starting **M0** and building test-fixture-driven **M1/M2**.

---

## 17. Official and primary technical references

Checked November 3, 2026. Read the latest versions again before implementation or release, especially for target agent directories and package publishing conventions.

### Skills and agent destinations

1. [Agent Skills specification](https://agentskills.io/specification) — skill directory, YAML frontmatter, naming, references/assets.
2. [Agent Skills official repository and reference library](https://github.com/agentskills/agentskills).
3. [Vercel Skills CLI command reference](https://github.com/vercel-labs/skills/blob/main/docs/commands.md) — source discovery, installation scope, agent adapters, copy/symlink conventions. **Reference for compatibility, not a dependency requirement.**
4. [OpenAI CLI installation / Codex paths](https://developers.openai.com/api/reference/cli).
5. [Cursor skills documentation](https://prod.cursor.com/docs/skills).
6. [Gemini CLI Agent Skills](https://geminicli.com/docs/cli/skills/).

### Rust and safe installation

7. [Rust `Result` error handling](https://doc.rust-lang.org/book/ch09-02-recoverable-errors-with-result.html).
8. [`clap` derive reference](https://docs.rs/clap/latest/clap/_derive/).
9. [`reqwest` documentation](https://docs.rs/reqwest/latest/reqwest/) — redirects/timeouts/TLS behavior.
10. [`zip` `enclosed_name` security guidance](https://docs.rs/zip/latest/zip/read/struct.ZipFile.html).
11. [`tar` safe `unpack_in` guidance](https://docs.rs/tar/latest/tar/struct.Entry.html).

### Distribution and provenance

12. [`dist` official documentation](https://axodotdev.github.io/cargo-dist/) — native releases and npm installers; tool may still be packaged under the historical `cargo-dist` name.
13. [`dist` configuration / installers](https://axodotdev.github.io/cargo-dist/book/reference/config.html).
14. [Maturin `bin` bindings](https://www.maturin.rs/bindings) — Rust executables distributed inside PyPI wheels.
15. [uv tools / `uvx`](https://docs.astral.sh/uv/guides/tools/).
16. [npm unscoped public package publishing](https://docs.npmjs.com/creating-and-publishing-unscoped-public-packages/).
17. [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/).
18. [GitHub artifact attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations).
19. [GitHub Actions security guidance](https://docs.github.com/en/actions/how-tos/secure-your-work).

---

**North-star success criterion:** A developer can run `npx thrn skills add uncharted`, `uvx thrn skills add uncharted`, or `thrn skills add uncharted` on a supported system, receive the same verified Rust CLI behavior, install a complete standards-compliant skill into an explicitly selected agent, inspect precisely what was installed, and safely update or remove it. Future Thrn resource types can reuse the same secure registry, source, planning, transaction, receipt, and release foundations.
