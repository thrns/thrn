# Architecture

## Current implementation

THRN is one Rust binary crate (`thrn`). The executable starts in `src/main.rs`, which calls the CLI and converts its result to a process exit code. `src/cli.rs` defines the Clap parser and dispatches `doctor`, offline skill discovery and listing, explicit catalog refresh, installed-listing checks, and local skill validation. `src/registry/remote.rs` performs the fixed HTTPS catalog request and exposes a bounded stream; `src/registry/catalog.rs` remains the sole catalog JSON validator; `src/registry/cache.rs` reads and atomically replaces the validated cache envelope; and `src/registry/catalog_service.rs` selects cache or bundled data and computes freshness. `src/registry/discovery.rs` builds an exact ID/alias index and performs stable text search, which the catalog listing reuses for canonical-ID ordering. The source pipeline is split into `src/sources/github.rs` for pinned HTTPS fetches, `selection.rs` for exact manifest-root and catalog-path matching, `staging.rs` for private extraction and lifetime ownership, and `archive.rs` for M2-01 ZIP validation. `src/install/receipt_store.rs` checks the future application-data receipt directory without creating it or interpreting a receipt format. `src/skills/validator.rs` reads and validates only a local skill's `SKILL.md`. `src/error.rs` carries CLI, catalog, refresh, receipt-discovery, resolution, validation, and output-I/O errors to the process boundary.

The runtime dependencies are intentionally small:

| Dependency | Current use |
|---|---|
| `clap` with `derive` | Typed command parsing and generated help/version/usage responses. |
| `serde` with `derive` | Strictly decode the external catalog schema into private raw parsing records. |
| `serde_json` | Bounded JSON decoding for the version-1 catalog parser. |
| `serde_path_to_error` | Add a schema field path to type errors so diagnostics can identify the location without echoing rejected JSON values. |
| `serde-saphyr` (deserialization only) | Parse bounded YAML frontmatter with duplicate-key, alias, merge-key, document, nesting, and unknown-tag controls. |
| `ureq` with only `rustls` enabled | Blocking HTTPS transport with certificate verification. Compression and unrelated HTTP features stay disabled; the client has explicit connect, body, and overall timeouts and follows no redirects. |
| `zip` with defaults disabled and only `deflate-flate2-zlib-rs` enabled | Parse ZIP metadata and stream Stored/Deflate entries for bounded size and CRC validation. Encryption and unrelated compression codecs are not compiled. |
| `directories` | Resolve standard per-user cache and application-data locations through XDG, macOS, and Windows conventions. |
| `tempfile` | Create a private temporary cache file beside the destination and atomically replace the cache where supported. |
| `thiserror` | Typed CLI-boundary errors for Clap, validation, and standard I/O failures. |
| `libc` (Unix only) | Set `O_NOFOLLOW` when opening `SKILL.md`; Windows uses the standard library's reparse-point flag. |

The standard library supplies process handling, environment constants, output, and validation collections. `Cargo.lock` records resolved crate versions. `ureq` 3.5.1 was selected as the maintained blocking client: Cargo reports Rust 1.85 as its MSRV, below THRN's Rust 1.99.1 minimum. The lockfile currently resolves its `rustls` feature and root certificates; defaults are disabled so gzip and other unused features are not compiled. The archive reader is `zip` 9.4.0, whose Rust 1.88 minimum is below THRN's Rust 1.99.1 toolchain. Default features are disabled and only the zlib-rs-backed Deflate feature is enabled. The validator supports Stored and Deflate compression methods.

The YAML parser is `serde-saphyr` 2.1.1, which declares Rust 1.89 as its MSRV; THRN pins Rust
1.99.1. Only the `deserialize` feature is enabled, with the crate's defaults disabled, so
serialization, comment capture, property interpolation, and `!include` support are not compiled
in. Its 2.1.1 release includes an empty-document parsing fix; its parser provides configurable
budgets and rejects duplicate mapping keys by default. THRN additionally sets explicit limits and
strict tag and merge-key policies for frontmatter.

## Current boundaries

`--help`, `--version`, `doctor`, `skills list`, `skills list --installed`, default `skills search`, `skills info`, and `skills validate` work offline. Only `skills search --refresh` calls the catalog transport and writes the cache. It uses one fixed HTTPS endpoint, verifies certificates, does not follow redirects, applies timeouts, and caps streamed catalog bytes at 1 MiB. All network data passes through the M1-01 parser before selection or persistence. The M2-02 source API accepts only a validated catalog `Resource`; it fetches the pinned commit archive, validates the streamed ZIP bytes before extraction, selects one exact catalog path, and returns a privately staged `StagedSkill`. Its narrow DOS ZIP compatibility rule accepts only DOS entries with zero external attributes, no directory marker, and `is_file() == true`; staged files are always non-executable. No CLI command installs or commits staged content to an agent directory. Automated tests inject network responses and temporary-directory locations; they use generated ZIPs and do not contact GitHub.

## Current and planned boundaries

Implemented boundaries:

- **M1-01:** `src/models/catalog.rs` contains validated catalog identity types; `src/registry/catalog.rs` parses bounded schema-v1 JSON without network or filesystem access. `docs/registry.md` documents the accepted schema and limits.
- **M1-02:** `src/registry/discovery.rs` builds exact canonical-ID and explicit-alias lookup indexes from validated records, and performs stable text search over the compile-time bundled catalog. The CLI exposes `skills search` and `skills info` without network access or skill downloads.
- **M1-03:** `src/skills/validator.rs` validates a local `SKILL.md` using bounded YAML parsing and typed frontmatter checks. It does not inspect supporting files, write files, or execute skill content.
- **M1-04:** `src/registry/remote.rs`, `cache.rs`, and `catalog_service.rs` separate HTTPS transport, persistent cache, and local-source/freshness selection. Default discovery uses a validated cache or bundled metadata without network access; only `--refresh` contacts the fixed endpoint.
- **M1-05:** `skills list` reuses `CatalogService` and `CatalogResolver` to show catalog availability in canonical-ID order. `skills list --installed` checks only whether the OS application-data receipt directory is absent or empty. No receipt schema or writer is introduced; M2-05 owns that design.
- **M2-01:** `src/sources/archive.rs` validates ZIP entry names, type metadata, collisions, resource budgets, local/central header consistency, expanded byte counts, and CRCs. Its manifest preserves the full archive-root prefix and contains metadata only.
- **M2-02:** `src/sources/github.rs`, `selection.rs`, and `staging.rs` implement pinned HTTPS fetching, one exact GitHub-to-codeload redirect check, manifest-based root and source-path selection, and isolated staging followed by M1-03 metadata validation. The pinned `frontend-design` archive passes the validator and staging pipeline with the approved DOS metadata fallback. Staged content stays owned by the returned `StagedSkill`; its `TempDir` guard attempts cleanup when dropped. This API does not install content or execute it. See [the source-fetch and staging boundary](security-model.md#m2-02-pinned-github-fetch-and-staging-boundary).

Future design boundaries, not current modules or capabilities:
- **M2-03 and later:** deterministic install plans, agent destinations, receipts, and recovery.
- **M3:** user scope, agent discovery, and receipt-backed check/update/remove operations with conflict handling.

The specification's long-term module map (`cli`, `models`, `registry`, `sources`, `skills`, `agents`, and `install`) is guidance for tested slices. Do not create empty modules to match that map. Command parsing and dispatch stay in the CLI layer; validation, source access, and filesystem transactions belong in their eventual domain boundaries.

Catalog source paths are validated lexically only. The parser does not establish whether a source exists or whether its content can be extracted safely; those checks belong to later fetching and installation slices.

## Adding commands

Add a Clap command variant and a small dispatch arm in `src/cli.rs`; put domain behavior in a module only when the slice needs it. Keep parsing separate from filesystem transactions. Preserve offline help/version behavior, successful output on stdout, diagnostics on stderr, and the existing `0`/`1`/`2` exit conventions documented in [the CLI contract](cli.md). Add integration coverage for the real executable's status and output channels, including offline operation. Never execute retrieved skill content as part of command dispatch.
