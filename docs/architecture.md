# Architecture

## Current implementation

THRN is one Rust binary crate (`thrn`). The executable starts in `src/main.rs`, which calls the CLI and converts its result to a process exit code. `src/cli.rs` defines the Clap parser and dispatches `doctor` and offline skill discovery. `src/doctor.rs` reports the crate version and Rust OS/architecture constants. `src/registry/discovery.rs` parses the compile-time bundled catalog through the catalog validator, builds an exact ID/alias index, and performs stable text search. `src/error.rs` carries CLI, catalog, resolution, and output-I/O errors to the process boundary.

The runtime dependencies are intentionally small:

| Dependency | Current use |
|---|---|
| `clap` with `derive` | Typed command parsing and generated help/version/usage responses. |
| `serde` with `derive` | Strictly decode the external catalog schema into private raw parsing records. |
| `serde_json` | Bounded JSON decoding for the version-1 catalog parser. |
| `serde_path_to_error` | Add a schema field path to type errors so diagnostics can identify the location without echoing rejected JSON values. |
| `thiserror` | Typed CLI-boundary errors for Clap and standard I/O failures. |

The standard library supplies process handling, environment constants, output, and validation collections. `Cargo.lock` records resolved crate versions. There is no runtime HTTP client, archive reader, temporary-directory crate, or installer dependency.

## Current boundaries

`--help`, `--version`, `doctor`, `skills search`, and `skills info` work offline. The CLI reads its arguments and writes to stdout or stderr; it does not inspect configuration or credentials, use the network, or invoke subprocesses. `main` is the only process exit boundary. The integration tests execute the compiled test binary from isolated temporary working directories; they do not contact live services.

## Current and planned boundaries

Implemented boundaries:

- **M1-01:** `src/models/catalog.rs` contains validated catalog identity types; `src/registry/catalog.rs` parses bounded schema-v1 JSON without network or filesystem access. `docs/registry.md` documents the accepted schema and limits.
- **M1-02:** `src/registry/discovery.rs` builds exact canonical-ID and explicit-alias lookup indexes from validated records, and performs stable text search over the compile-time bundled catalog. The CLI exposes `skills search` and `skills info` without network access or skill downloads.

Future design boundaries, not current modules or capabilities:

- **M1-03:** Agent Skills `SKILL.md` metadata validation.
- **M1-04:** trusted catalog fetching and caching.
- **M2:** bounded source fetching, hostile archive/path validation, agent destinations, deterministic install plans, same-filesystem staging, receipts, and recovery.
- **M3:** user scope, agent discovery, and receipt-backed check/update/remove operations with conflict handling.

The specification's long-term module map (`cli`, `models`, `registry`, `sources`, `skills`, `agents`, and `install`) is guidance for tested slices. Do not create empty modules to match that map. Command parsing and dispatch stay in the CLI layer; validation, source access, and filesystem transactions belong in their eventual domain boundaries.

Catalog source paths are validated lexically only. The parser does not establish whether a source exists or whether its content can be extracted safely; those checks belong to later fetching and installation slices.

## Adding commands

Add a Clap command variant and a small dispatch arm in `src/cli.rs`; put domain behavior in a module only when the slice needs it. Keep parsing separate from filesystem transactions. Preserve offline help/version behavior, successful output on stdout, diagnostics on stderr, and the existing `0`/`1`/`2` exit conventions documented in [the CLI contract](cli.md). Add integration coverage for the real executable's status and output channels, including offline operation. Never execute retrieved skill content as part of command dispatch.
