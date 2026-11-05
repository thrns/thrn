# Security model

## Current exposure

THRN is under development and has no supported installer release. The current binary parses arguments, prints help/version, reports static build metadata through `doctor`, reads validated local catalog data, offers an explicit HTTPS catalog refresh, performs read-only local `SKILL.md` validation, and checks the future receipt directory for installed listing. Default discovery and both `skills list` modes never use the network. Explicit refresh contacts one fixed THRN-operated HTTPS endpoint, validates the response, and writes only the catalog cache. No command fetches skills or executes subprocesses. A library API can inspect a caller-supplied ZIP byte slice without writing its contents. The installed listing reads only THRN's application-data receipt directory: it does not inspect agent skill folders, create receipt storage, or interpret a provisional receipt format. Installation, durable receipts, target ownership, archive extraction, staging, and rollback protections are not implemented; their absence is not a security guarantee for an installer.

## Boundaries required before installation

Registry metadata, GitHub responses, archives, skill files, and instructions inside `SKILL.md` must be treated as untrusted input. A skill is content for a compatible agent, not an instruction source for THRN. Installing or inspecting one must never implicitly invoke scripts, hooks, shells, package managers, or other retrieved code.

Before M2 installation work is accepted, source and filesystem handling must:

- Catalog refresh is limited to `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`; require HTTPS certificate verification, reject redirects, use explicit timeouts, and enforce the 1 MiB limit while streaming. Validate UTF-8 and the full catalog schema before caching. This endpoint is curated by THRN, but HTTPS and catalog inclusion do not authenticate or establish the safety of individual skill publishers. For later skill fetching, use public GitHub sources at exact, full commit hashes and independently validate source origin and immutable revision. A checksum from the same registry can detect accidental changes but does not prove publisher identity if that registry is compromised.
- Bound network time, response size, archive size, expanded bytes, file count, per-file size, and path depth. Reject malformed metadata, absolute or traversal paths, drive/UNC prefixes, unsupported file types, case-fold collisions, and symlinks or hardlinks that escape the staging or target root.
- Validate Agent Skills frontmatter and the skill directory/name relationship before installation, while preserving legitimate supporting files.
- Treat an existing unreceipted target as externally managed. Never overwrite or recursively remove unrelated or user-modified paths. Require explicit scope and conflict choices; project scope is the default, user scope must be selected, and ambiguous noninteractive agent selection must fail.
- Stage complete content near each destination and use atomic rename where the filesystem supports it. Multiple destinations are a recoverable sequence, not one cross-target atomic transaction. Write durable ownership receipts only after successful commits; preserve recovery state and report incomplete rollback.
- Avoid logging credentials, tokens, environment secrets, private skill content, or unnecessary personal paths. The v1 public-source workflow should not require a GitHub token. Any future credential support needs an explicit design and redaction tests.
- Make supply-chain claims proportionate to evidence. Pinned commits identify source revisions, not author identity. Stronger publisher authenticity requires a separately reviewed signing or attestation design; do not imply that TLS or a same-origin digest provides it.

## Milestone status

### M2-01 archive validation boundary

`src/sources/archive.rs` accepts ZIP archives only. The validator reads an in-memory byte slice, parses central and local entry metadata, checks local/central header consistency and overlap, and streams each regular file through a fixed 8 KiB buffer to verify actual expanded byte counts and the ZIP CRC-32. It does not retain file data, execute scripts, fetch an archive, extract files, or choose the catalog's `source.path`.

The default resource budgets are:

| Limit | Default |
|---|---:|
| Compressed archive bytes, including ZIP metadata | 32 MiB |
| Explicit archive entries | 4,096 |
| Manifest paths, including implied directories | 4,096 |
| Expanded bytes per regular file | 16 MiB |
| Expanded bytes across regular files | 128 MiB |
| Path bytes, excluding a directory's final `/` marker | 1,024 |
| Bytes per path component | 255 |
| Path component depth | 32 |

Callers may supply stricter or different `ArchiveLimits`; all byte totals use checked arithmetic. Entry counts are preflighted from the ZIP end records before the ZIP parser allocates its entry index. Expansion limits are enforced while reading, regardless of declared uncompressed sizes. A ZIP entry whose actual output differs from its declared size, whose CRC fails, or whose compressed stream is truncated is rejected, and no partial manifest is returned.

Archive names use a platform-independent portable policy. Paths must be ASCII and slash-separated. The validator rejects absolute and leading-separator paths; `.` and `..`; empty components; backslashes; drive and UNC forms; NUL and control characters; colons/alternate data streams; Windows-invalid filename punctuation; reserved Windows device names (including names with extensions); trailing spaces or dots; and paths, components, or nesting beyond the limits above. It validates the exact input spelling and never strips or resolves path components. Exact duplicates, ASCII case-fold collisions, and file/directory conflicts with explicit or implied parent directories are rejected. ASCII-only names are a v1 portability constraint, so valid Unicode archive names are currently rejected.

Only ordinary regular files and directories are accepted. ZIP entries must have identifiable, consistent file/directory types and must not declare data in a directory. Symlinks, special Unix file types, ASi Unix link-capable extra fields, encrypted entries (including encryption-related extra fields), unsupported compression methods, and malformed extra-field metadata are rejected. Stored and Deflate compression are supported. A legitimate `scripts/` directory is treated as ordinary file content and is never executed. The ZIP format does not define a portable hardlink entry type; known ASi Unix link metadata is rejected, but opaque vendor-specific metadata cannot be assigned link semantics without understanding that vendor format.

The returned manifest lists explicit regular files and directories plus implied parent directories. Paths remain relative to the ZIP archive root. A GitHub-style generated top-level repository directory remains visible in the manifest; the validator neither strips prefixes nor guesses which nested directory matches a catalog source path. The manifest is evidence that this byte slice passed these parsing, path, type, resource, and CRC checks. It does not authenticate a GitHub publisher, verify a pinned repository commit, establish that content is benign, or guarantee that a later installation is safe.

Network response bounds and immutable GitHub commit/origin verification belong to M2-02. Exact catalog `source.path` selection, safe extraction, destination validation, filesystem race protection, staging, ownership, and recovery belong to M2-02 and later implementation slices. Since this API receives an already materialized byte slice, its compressed-size check does not bound the caller's allocation; a future fetcher must enforce the archive limit while receiving bytes as well. CRC-32 detects common corruption but is not a cryptographic digest or publisher-authentication mechanism.

Implemented in M1-01 through M2-01:

- **Catalog boundary:** version-1 JSON is size- and count-bounded; required fields, identifiers,
  case-insensitive ID/alias collisions, GitHub metadata, full commit hashes, and source-path
  lexical rules are validated before typed data is returned. The parser performs no network or
  filesystem operations. See [the catalog schema](registry.md).
- **Discovery boundary:** exact lookup accepts canonical IDs and explicit aliases from validated
  records; the skill directory name is not an implicit identifier. Search and info read only the
  bundled static catalog. A pinned commit identifies a revision but does not guarantee publisher
  authenticity.
- **Local metadata boundary:** M1-03 opens one regular `SKILL.md`, caps it at 256 KiB, rejects file
  symlinks and special files, and parses the YAML frontmatter with bounded depth, nodes, events,
  one-document, duplicate-key, tag, merge-key, anchor, and alias controls. It does not read
  supporting files or validate scripts, and it never writes or executes skill content. Metadata
  validation does not establish publisher authenticity or the safety of a skill's instructions or
  scripts. See [local validation limits](registry.md#local-agent-skills-validation).
- **Catalog network/cache boundary:** M1-04 uses a Rustls-backed HTTPS client for the fixed URL,
  verifies certificates, follows no redirects, and applies connect, response-body, and overall
  timeouts. It bounds response reads independently of `Content-Length` and passes bytes through the
  existing catalog parser before persistence. One source-tagged cache file is loaded only after
  envelope and catalog revalidation; symlinks and nonregular cache files are rejected. Refresh
  writes via a same-directory temporary file and atomic replacement. Failed fetches, validation,
  or commits do not replace the last valid cache. Offline discovery uses cache or bundled metadata
  and never silently refreshes stale content.
- **Listing boundary:** M1-05's catalog list reuses the validated local catalog selection and exact
  resolver. The installed list considers only whether the OS application-data `receipts` directory
  is missing or empty. Existing content is an error until M2-05 defines receipt schema and
  ownership; no installation records are inferred from catalog data or agent folders. Listing
  creates no directories and writes no files.
- **Archive boundary:** M2-01 applies the ZIP-only, metadata-only validation described above. The
  validator preserves GitHub archive root prefixes and has no network, extraction, install, or
  publisher-authentication behavior.

## Non-blocking M1-04 follow-up review

The M1-04 cache implementation has two follow-up topics for a later security review; they do not
block offline listing and are not expanded in M1-05:

- **Filesystem hardening:** review path-component and concurrent-replacement races around cache
  directory validation and atomic replacement, and decide whether parent-directory durability
  failures should be surfaced instead of best-effort ignored.
- **Recovery messaging:** refine the cache-fallback diagnostic so it distinguishes corrupt cache
  data from unsafe or inaccessible cache storage, and explains what refresh can repair while
  making clear that failed refreshes preserve the prior cache.

Remaining requirements:

- **M2-02 and later:** constrained GitHub fetching, exact source-directory selection, safe extraction,
  destination checks, ownership receipts, staged installation, and rollback/recovery.
- **M3:** safe user-scope support and receipt-backed update/remove behavior that detects modified content and reports partial failure.

Security-sensitive parsing and writes require negative tests with hostile fixtures, isolated temporary directories, and deterministic network mocks. See the [roadmap](ROADMAP.md) and [repository security policy](../SECURITY.md) for project status.
