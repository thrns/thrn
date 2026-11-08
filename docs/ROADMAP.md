# THRN delivery roadmap — incrementally reviewable tasks

Source of truth: `THRN_PROJECT_SPEC.md` §§0–14. Implement a real vertical slice each cycle. **Each task:** scoped code + relevant positive/negative tests + documentation + `bash scripts/verify.sh` + CI green + independent architectural acceptance. A command is not "done" merely because it parses.

## M0 — Foundation (dependency for all work)

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M0-01 | bootstrap | Real `clap` CLI with offline `--help`, `--version`, `doctor` placeholder and typed error handling; integration tests for success/unknown subcommands |
| M0-02 | M0-01 | Correct exit/error and output channel conventions, test harness for CLI stdout/stderr and isolated tempdirs; no network in smoke tests |
| M0-03 | M0-02 | Architecture/CLI/security docs, release gates baseline; CI on Ubuntu/macOS/Windows and advisory job green |

## M1 — Standards-aware discovery

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M1-01 | M0 | Typed versioned catalog `schemaVersion=1`, validation of duplicate IDs/aliases and exact 40-hex pinned commits; invalid fixtures rejected |
| M1-02 | M1-01 | Local built-in catalog resolver; `skills search` and `skills info`; explicit alias `uncharted` to canonical skill name; fixture responses |
| M1-03 | M1-01 | Agent Skills `SKILL.md` YAML frontmatter validation (`skills validate <path>`); name/folder matching, malformed YAML, overlong metadata, supporting files tests |
| M1-04 | M1-02 | Trusted HTTPS catalog fetch/cache with timeout/size bounds and stale/offline indication; deterministic fake HTTP fixtures, no live test dependency |
| M1-05 | M1-02, M1-04 | Distinguish catalog listing and `skills list --installed`; offline listing from empty receipts passes without network |

## M2 — Secure installation, Codex target first

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M2-01 | M1-01, M1-03 | Hardened archive/path/file-type validator; reject traversal, Windows prefixes, symlink escapes, collisions, oversized archives, corruption |
| M2-02 | M2-01, M1-04 | Public GitHub pinned-commit fetcher; restrictive host redirects/size/time bounds, exact directory extraction, isolated fake HTTP tests |
| M2-03 | M1-02, M1-03 | Pure immutable install-plan builder; project target, collision detection, `--dry-run` prints exact changes and leaves filesystem unchanged |
| M2-04 | M2-03, M2-01 | Internal Codex project adapter with retained staging lifetime, destination revalidation, exclusive copy, SHA-256 verification, and platform no-replace directory commitment; CLI remains dry-run-only |
| M2-05 | M2-04 | Strict schema-v1 receipts keyed by canonical project/agent/resource identity; bounded SHA-256 inventories, no-follow/no-replace durable publication, validated offline installed listing; CLI installation remains dry-run-only |
| M2-06 | M2-05 | Recovery/rollback for partial or failed install; inject rename/write failure and verify no successful receipt or clobber; only then consider enabling public installation |
| M2-07 | M2-06 | Enable `skills install` for explicit Codex project scope by connecting the validated install session to recovery-aware commit-and-record; test offline orchestration, receipt listing, duplicate/unmanaged conflicts, partial failures and recovery; document that remove/update are not yet public |

## M3 — Lifecycle and agent coverage

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M3-01 | M2-06 | `skills remove`: explicit Codex project scope; exact canonical ID, schema-v2 installation-time destination identity and complete current inventory verification; schema-v1 receipts remain listable but cannot authorize removal; quarantined no-replace removal with a separate versioned journal, per-object cleanup intents, receipt retirement after content removal; deterministic fault and subprocess-crash recovery tests |
| M3-02 | M3-01 | `skills update`: exact canonical ID and schema-v2 ownership; unchanged GitHub owner/repository/source path/skill name; newer full pinned commit; immutable file-level plan; destination-local staging and old-version backup; journaled no-replace receipt transition; verified forward recovery and cleanup; offline fixture, crash, failure, concurrency, CLI, and platform tests; no partial-success claims |
| M3-03 | M3-02 | `skills check`: read-only update discovery, offline behavior and exit semantics tested |
| M3-04 | M2-06 | User/global scope and OS config/data/cache paths; temp-home isolation; no global writes without selection |
| M3-05 | M3-04 | Claude Code adapter: correct target path; all scope-specific add/remove/conflict tests |
| M3-06 | M3-05 | Cursor and Gemini adapters; shared `.agents/skills` deduplication when supported; platform/agent discovery tests |
| M3-07 | M3-06 | Multiple-agent targeting, explicit/ambiguous `--no-input`, target conflict matrix; rollback across targets documented and tested |
| M3-08 | M3-02, M3-07 | Per-target locking + concurrency/recovery tests for overlapping installs, updates and removes |
| M3-09 | M3-08 | `--json`, `--quiet`, `--verbose`, exit codes, `NO_COLOR`, `CI`; valid JSON-only stdout, informative stderr; `doctor` real read-only checks |

## M4 — Native distribution

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M4-01 | M3 complete | Review and configure `dist` for native binaries; release plan and metadata checked, no publishing |
| M4-02 | M4-01 | GitHub matrix build artifacts/sha256 checksums; clean Linux/macOS/Windows install-and-run smoke tests |
| M4-03 | M4-02 | Signed/restricted release workflow, provenance/attestation where available; prevent untrusted PR publishing |

## M5 — npm + PyPI native wrappers

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M5-01 | M4-02 | `dist` npm installer artifact (same Rust program), package name availability and unpacked tarball checks; local npx smoke test |
| M5-02 | M4-02 | `maturin` binary wheels, platform tags and entry point verified in isolated Python/uv env |
| M5-03 | M5-01, M5-02, M4-03 | Verified npm/PyPI package ownership, OIDC trusted publishing configuration, version lockstep, permission-constrained publishing (explicit approval) |
| M5-04 | M5-03 | Clean published `npx thrn`, `uvx thrn`, and native parity on supported systems; no release claim until verified |

## M6 — Public launch

| Task | Depends on | Deliverable and independently testable acceptance |
|---|---|---|
| M6-01 | M3 | CLI reference, security model, troubleshooting, contributor and license/docs accuracy against actual running commands |
| M6-02 | M5-04 | Static hosted docs/catalog generated from same registry; no backend dependency and no stale duplicate catalog |
| M6-03 | M6-01, M6-02 | Release runbook, public install/remove smoke test, checksums, links, changelog, support matrix, disclosure policy |

### Security test classes to carry through M1–M3

Traversal (`../`, absolute, Windows drive/UNC), symlink escape and cycles, case-insensitive collision, malformed YAML/invalid SHA, zip bombs/bounded decompression, timeouts and redirects, foreign/unwritable destinations, interrupted writes/rollback, modified managed content, receipt corruption, concurrency, offline operation, noninteractive ambiguity, and JSON stdout purity.

### Explicit exclusions

No native Bun/npm-style JS runtime, library dependency solver, private Git token management, dynamic community publishing, plugin executable loading, mandatory website/server/database, ChatGPT Web direct skill install, automatic untrusted script execution, or release without actual package ownership.
