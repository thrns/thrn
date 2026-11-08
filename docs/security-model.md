# Security model

## Current exposure

THRN is under development and has no supported package-manager release. The current binary parses arguments, prints help/version, reports static build metadata through `doctor`, reads validated local catalog data, offers an explicit HTTPS catalog refresh, performs read-only local `SKILL.md` validation, lists validated receipt-backed installations, checks selected Codex, Claude Code, Cursor, or Gemini CLI project/user skills offline, and plans/installs or updates/removes exactly verified receipt-backed skills in either explicitly selected scope. Project scope remains the default. User scope is selected only with `--scope user`. Default discovery, both `skills list` modes, `skills check`, and `skills remove` never use the network. Explicit refresh contacts one fixed THRN-operated HTTPS endpoint, validates the response, and writes only the catalog cache. Install and update commands read the local catalog without refreshing it, then fetch the selected skill at its pinned public GitHub commit and validate it privately. Install/update dry-runs print plans without changing skill or receipt data. All four agents and both scopes use the same commit, receipt, journal, lock, and recovery boundaries. No retrieved or installed content, script, or subprocess is executed. Installed listing reads only THRN's application-data receipt directory: it does not inspect agent skill folders, create receipt storage, fetch the catalog, open receipt-referenced destinations, or mutate recovery state. It labels current content integrity as unchecked and fails closed while lifecycle recovery is pending. See [the recovery protocol](recovery.md) for transaction and durability limits.

## Scope and operating-system paths

`InstallScope` is a typed selection shared by install planning, install commit, read-only check,
receipt lookup, update, removal, and recovery. Project remains the default and resolves the selected
existing project (current directory unless `--project` is supplied). Codex uses `<project>/.agents/skills/<validated-skillName>` and
`<home>/.agents/skills/<validated-skillName>`; Claude Code uses `.claude/skills`, Cursor uses
`.cursor/skills`, and Gemini CLI uses `.gemini/skills`, each below the selected project or validated home. Explicit user scope resolves only the validated
OS home; it does not use the working directory.
`--scope user --project <path>` is rejected. The catalog resource ID remains separate from the
validated skill directory name. No scope is inferred from an existing skill.

`OsPaths` centralizes user home, application config, application data, and catalog cache resolution.
Unix-like systems use `HOME` and Windows uses `USERPROFILE`, with the platform home-directory API
as fallback. Platform-standard configuration, data, and cache locations remain in use; absolute
`THRN_DATA_DIR` and `THRN_CACHE_DIR` overrides keep their existing purpose and do not change the
skill destination. A missing, nonabsolute, ambiguous, or unresolvable home fails closed. Home path
components are opened without following Unix symlinks or Windows reparse points. THRN has no
arbitrary destination override and does not fall back to another folder if the expected user skill
root is unavailable.

Codex user ownership uses schema-v3 receipts; Claude Code project/user ownership uses schema-v4;
Cursor and Gemini CLI use schema-v5 receipts that bind agent, scope, destination layout, root and
destination identities, provenance, and the complete hash inventory. Existing Codex project
schema-v1/v2 keys remain unchanged. All agents and scopes use the same receipt store, OS-backed
writer lock, install/remove/update journals,
no-replace operations, object-specific cleanup intents, and recovery algorithms. Existing unmanaged
skills and legacy command files are never adopted. `$CODEX_HOME/skills` is not a THRN destination.
Agent selection remains explicit. Read-only PATH capability inspection reports command-line executable presence only; it is not used for auto-selection and does not prove that an agent loads a destination. No destination migration is performed.

Claude Code destinations are fixed to `.claude/skills` in the selected project or user home.
`CLAUDE_CONFIG_DIR` is not treated as a skill-root override: official settings documentation says
it relocates settings, session history, and plugins, but does not document relocation of standalone
skills. THRN therefore continues to use `.claude/skills` and `~/.claude/skills`. The Claude adapter
rejects host-reserved skill names, including `synced` and the `anthropic-skills` namespace.

## Shared Cursor and Gemini discovery

The official [Cursor skills documentation](https://cursor.com/docs/skills) and
[customization help](https://cursor.com/help/customization/skills) document `.agents/skills`
compatibility alongside `.cursor/skills`. Cursor's native user skill directory is used for cloud
skill sync; shared `.agents/skills` is not automatically synced to Cursor Cloud Agents. Because the
documentation does not establish same-name precedence, THRN fails closed on a shared same-name
collision rather than choosing which copy Cursor loads.

Gemini CLI's official [skills documentation](https://geminicli.com/docs/cli/skills/) and
[repository](https://github.com/google-gemini/gemini-cli) document `.agents/skills` compatibility
and precedence over `.gemini/skills` within the same discovery tier. THRN therefore rejects a native
Gemini install when the same skill name already exists at `.agents/skills` in that selected scope.

For either adapter, compatibility inspection is bounded, no-follow, read-only, and limited to the
explicitly targeted skill name. It distinguishes an exact shared skill verified against a Codex
receipt, unmanaged external content, and an absent shared path. A Codex-managed skill may already be
discoverable by Cursor or Gemini, but its update and removal remain Codex-owned operations. THRN
never adopts, overwrites, moves, links, clones, removes, or creates another receipt for shared
content. The shared-path snapshot is checked before mutation and revalidated at commit; a changed
shared directory fails closed. Visibility through another agent's root is not reported as THRN
ownership by the selected agent.

Path tests inject isolated `HOME`, `USERPROFILE`, platform data/cache variables, and THRN data roots.
The CLI tests do not read or write the developer's real user skill directory.

## Read-only update checking

`thrn skills check` compares receipts and installed trees for the selected agent and scope with the
catalog selected by `CatalogService::discover()`. Codex remains the default when `--agent` is
omitted; other agents require their explicit `--agent` IDs. There is no interactive or automatic target selection.
`--scope user` selects receipts rooted at the validated OS home. It uses a validated local cache when
available and otherwise bundled metadata; stale cache data remains usable but is labeled stale.
Bundled metadata and stale cache values are not claims about the latest GitHub state. The check
never refreshes the catalog, sends network requests, stages GitHub content, executes skill files,
starts lifecycle transactions, or attempts recovery. It creates no application-data directory or
lock file and leaves skill, receipt, and journal contents unchanged. It may temporarily acquire
the existing OS-backed receipt-store lock. An active writer, pending install/remove/update journal,
corrupt or unsafe receipt storage, or invalid catalog data prevents a trustworthy result and
returns an error. A modified or structurally unsafe selected-scope installation is reported as an
ineligible finding when its receipt metadata can be safely read.

Check reads receipt metadata for storage validation but opens and verifies installed trees only
for the selected scope root. Results are ordered by canonical resource ID and skill name.
For each installation it reports the installed receipt SHA and local catalog SHA when available:

- **Up to date** means scope-appropriate receipt identity and full tree verification succeed, catalog
  identity fields match, and both pins are equal.
- **Update available** means the scope-appropriate identity and full tree verify, resource ID, owner, repository,
  source directory, and skill name match, and the local catalog pins a different full commit SHA.
  A different SHA only means a different catalog-pinned revision; it does not establish Git
  ancestry or commit chronology.
- **Not in catalog** means the receipt's canonical ID has no exact entry in the selected catalog.
- **Source mismatch** means the entry exists but its owner, repository, source directory, or skill
  name changed. The automatic updater refuses that identity change.
- **Legacy receipt** means a verified schema-v1 receipt has no installation-time destination
  identity and cannot be used by the automatic updater.
- **Modified or unsafe installation** means the receipt-backed destination identity, contents, or
  structure did not verify. Such a finding is never eligible for automatic update.

`skills check` exits `0` after a completed check even when an update is available or a resource is
not cataloged. Integrity, filesystem, catalog, and recovery errors exit `1`; usage errors exit `2`.
To perform an explicit update, review the finding and run
`thrn skills update <canonical-id> --agent <codex|claude-code|cursor|gemini>`. Update repeats receipt/tree checks and may fetch
the exact locally pinned archive; check does neither.

## Current installation security boundaries

Registry metadata, GitHub responses, archives, skill files, and instructions inside `SKILL.md` must be treated as untrusted input. A skill is content for a compatible agent, not an instruction source for THRN. Installing or inspecting one must never implicitly invoke scripts, hooks, shells, package managers, or other retrieved code.

The enabled installation path follows these source and filesystem boundaries:

- Catalog refresh is limited to `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`; require HTTPS certificate verification, reject redirects, use explicit timeouts, and enforce the 1 MiB limit while streaming. Validate UTF-8 and the full catalog schema before caching. This endpoint is curated by THRN, but HTTPS and catalog inclusion do not authenticate or establish the safety of individual skill publishers. Skill fetching uses public GitHub sources at exact, full commit hashes and independently validates source origin and immutable revision. A checksum from the same registry can detect accidental changes but does not prove publisher identity if that registry is compromised.
- Bound network time, response size, archive size, expanded bytes, file count, per-file size, and path depth. Reject malformed metadata, absolute or traversal paths, drive/UNC prefixes, unsupported file types, case-fold collisions, and symlinks or hardlinks that escape the staging or target root.
- Validate Agent Skills frontmatter and the skill directory/name relationship before installation, while preserving legitimate supporting files.
- Treat an existing unreceipted target as externally managed. Never overwrite or recursively remove unrelated or user-modified paths. Install, update, and removal require an explicit supported `--agent`; project is the default scope, and user scope requires explicit `--scope user`. Destination conflicts fail without overwrite.
- Stage complete content near each destination and use atomic rename where the filesystem supports it. Multiple destinations are a recoverable sequence, not one cross-target atomic transaction. Write durable ownership receipts only after successful commits; preserve recovery state and report incomplete rollback.
- Avoid logging credentials, tokens, environment secrets, private skill content, or unnecessary personal paths. The v1 public-source workflow should not require a GitHub token. Any future credential support needs an explicit design and redaction tests.
- Make supply-chain claims proportionate to evidence. Pinned commits identify source revisions, not author identity. Stronger publisher authenticity requires a separately reviewed signing or attestation design; do not imply that TLS or a same-origin digest provides it.

The removal path adds its own checks and has narrower authorization than a catalog install:

- Require an explicit supported `--agent` and an explicitly resolved scope, and an exact canonical resource ID from one
  validated ownership receipt. Removal does not load catalog data, accept aliases, infer identity
  from `skillName`, or operate on unreceipted content.
- Match the selected scope root and its recorded filesystem identity, receipt agent and
  scope, and exact agent-specific native destination (`.agents/skills` for Codex, `.claude/skills` for Claude Code, `.cursor/skills` for Cursor, or `.gemini/skills` for Gemini CLI). Open every path component
  without following links/reparse points. Reject case-insensitive collisions, unexpected object
  types, duplicate filesystem identities, hard-linked files, missing/extra entries, changed bytes,
  and file hashes or lengths that differ from the receipt.
- Hold the existing OS-backed receipt-store writer lock through recovery, revalidation, mutation,
  receipt retirement, and journal cleanup. Installation admission checks removal journals under
  the same lock, so an unfinished transaction blocks another install/remove operation.
- Persist a separate versioned removal journal before moving the destination. Move it to a random
  same-filesystem sibling quarantine with a no-replace rename. Recheck every file and identity before
  deleting it, durably record an object-specific cleanup intent first, and remove a directory only
  after its expected identity matches and it is empty. Keep the receipt until content removal has
  been established; a failure or uncertain state is nonzero and cannot print success.
- Do not execute installed skill files, scripts, hooks, or instructions during verification,
  removal, or recovery. Leave agent parent directories in place.

The update path has its own constraints:

- Require an exact canonical resource ID and matching Codex project schema-v2/user schema-v3, Claude schema-v4, or Cursor/Gemini schema-v5 receipt, with matching scope root, agent,
  scope, source owner/repository/directory, skill name, and installation-time destination identity.
  Do not refresh the catalog or accept caller-supplied URLs.
- Verify every current object identity, file length, and SHA-256 against the receipt before fetching
  a different catalog-pinned source. Reject changed, extra, missing, linked, colliding, replaced, or ambiguous content,
  schema-v1 receipts, repository transfers, and renamed sources.
- Stage and verify the pinned replacement on the destination filesystem. Move the verified old tree
  to an operation-owned sibling backup with no-replace semantics, then commit the new tree with a
  second no-replace rename. These renames do not form an atomic swap.
- Preserve the exact old receipt in an operation-specific backup. Record its raw-byte digest and
  filesystem identity, the new receipt contents, and the publication temp identity in a distinct
  update journal. Publish through the existing no-replace receipt API; never overwrite a receipt.
- Hold the shared writer lock through recovery, revalidation, directory commitment, receipt
  transition, verification, and cleanup. Installed listing holds the same lock and fails closed
  while update journals remain. Recovery deletes only individually verified backup objects after
  durable per-object intents; ambiguous or modified objects are preserved.

Schema-v1 receipts record project-root filesystem identity and full content digests, but they do
not persist the installed directory's filesystem identity at installation time. The removal journal
captures the verified current destination identity before mutation and protects the rest of the
transaction from substitutions. Proposed ADR-013 leaves the sufficiency of that evidence boundary
for independent architectural review. On Windows, the current secure handle layer cannot portably
flush directory-entry metadata; a power loss can therefore leave rename/unlink durability uncertain.

M2-03 implements read-only destination checks and an immutable proposal. M2-04 adds the initial Codex project directory commit; M2-05 adds post-commit ownership receipts and receipt-backed listing; M2-06 adds journaled recovery; M2-07 connects project install to `prepare_install_session()` and `commit_and_record()`. M3-01 adds receipt-backed removal, M3-02 adds verified update, and M3-03 adds offline read-only check. M3-04 extends the lifecycle to explicitly selected Codex user scope and centralizes OS path resolution. M3-05 adds Claude Code and M3-06 adds Cursor/Gemini project/user support using the same engine. Aliases and unreceipted directories are not resolved or adopted, and no command refreshes the catalog implicitly. Codex project schema-v1/v2 and user schema-v3 receipts and identity formats remain unchanged; Claude uses strict schema v4 and Cursor/Gemini use strict schema v5. Lifecycle operations use separate versioned journals and report incomplete transactions without a success block. ADR-015 and ADR-016 are approved; ADR-017 remains proposed pending independent architectural review. This documentation does not claim M3-06 architectural acceptance or a supported package release.

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

Only ordinary regular files and directories are accepted. ZIP entries must have identifiable, consistent file/directory types and must not declare data in a directory. The single compatibility exception is an entry whose creator system is DOS, external attributes are exactly zero, no directory marker is present, and `ZipFile::is_file()` is true; it is classified as an ordinary regular file. This matches the pinned GitHub archive's encoding for ordinary DOS-created files, which carries no Unix type bits. Staging writes accepted files as regular files with executable permissions discarded. Unix entries without type bits, symlinks, special Unix file types, DOS directory-bit/name mismatches, ASi Unix link-capable extra fields, encrypted entries (including encryption-related extra fields), unsupported compression methods, and malformed extra-field metadata remain rejected. Stored and Deflate compression are supported. A legitimate `scripts/` directory is treated as ordinary file content and is never executed. The ZIP format does not define a portable hardlink entry type; known ASi Unix link metadata is rejected, but opaque vendor-specific metadata cannot be assigned link semantics without understanding that vendor format.

The returned manifest lists explicit regular files and directories plus implied parent directories. Paths remain relative to the ZIP archive root. A GitHub-style generated top-level repository directory remains visible in the manifest; the validator neither strips prefixes nor guesses which nested directory matches a catalog source path. The manifest is evidence that this byte slice passed these parsing, path, type, resource, and CRC checks. It does not authenticate a GitHub publisher, verify a pinned repository commit, establish that content is benign, or guarantee that a later installation is safe.

Since this API receives an already materialized byte slice, its compressed-size check does not bound the caller's allocation; M2-02 therefore enforces the same 32 MiB cap while receiving the response. CRC-32 detects common corruption but is not a cryptographic digest or publisher-authentication mechanism.

### M2-02 pinned GitHub fetch and staging boundary

The source API accepts only a validated catalog `Resource`. It constructs the archive URL from
that resource's typed GitHub owner, repository, and full commit SHA; callers cannot supply a URL.
The initial request is `https://github.com/{owner}/{repository}/archive/{full-sha}.zip`. Redirects
are disabled in the HTTP client. The fetcher accepts only one HTTP 302 whose `Location` is exactly
`https://codeload.github.com/{owner}/{repository}/zip/{full-sha}`; every other redirect, host,
scheme, path, query, or second redirect fails. The codeload destination was observed when checking
the pinned `frontend-design` catalog archive. GitHub documents commit-addressed archive URLs and
recommends its archives API for stable commit content: [GitHub source code archive documentation](https://docs.github.com/en/repositories/working-with-files/using-files/downloading-source-code-archives).

The Rustls-backed `ureq` transport verifies TLS certificates, permits HTTPS only, requests no
GitHub credentials, and applies 5-second connect, 15-second response-body, and 30-second overall
timeouts. It accepts only HTTP 200 with `application/zip`, no content encoding (or `identity`),
and a single valid `Content-Length` when present. It streams into memory with a hard 32 MiB cap,
including when the response is chunked or omits its length, and requires the received byte count to
match the declared length. Private repositories are unsupported and no authentication fallback is
attempted. Request errors do not include URLs, headers, proxy configuration, or environment values.

The exact downloaded byte vector is passed to M2-01's `validate_zip_archive` before any extraction
write. Selection uses only its validated manifest: there must be one explicit top-level directory
named exactly `{repository}-{full-sha}`, that root must match the requested source, and there must
be no entries under another top-level name. The catalog `source.path` is matched exactly beneath
that root, with capitalization differences rejected. A regular `SKILL.md` must exist directly in
the selected directory. No other directory is searched.

Only manifest-authorized regular files and directories below the selected path are copied into an
unpredictable, exclusive `tempfile` directory owned by the operation. Paths are rebuilt from
validated components; directory creation does not reuse existing paths, file creation is exclusive,
copies are bounded by manifest sizes, and file contents are re-read from the same immutable archive
bytes with size and ZIP integrity checks. On Unix, staged directories use mode `0700` and files use
`0600`; archive executable bits are discarded. A failed operation closes and cleans the temporary
directory. The returned `StagedSkill` owns the `TempDir`; its content path is valid only while that
value stays alive and the content is removed when it is dropped. The staged result is returned only
after the existing M1-03 validator accepts its `SKILL.md` and the metadata name equals catalog
`skillName`. The returned `StagedSkill` owns a `TempDir` guard; callers must keep the value alive
while using its content path, and the guard attempts cleanup when dropped. Supporting files remain
opaque and are never executed.

These are separate evidence layers:

- **Trusted THRN catalog metadata** supplies the owner, repository, source path, expected skill
  name, and full commit SHA used by this operation.
- **Pinned GitHub repository revision** identifies the requested source state; it does not prove
  who authored that revision.
- **Validated archive structure** means the downloaded bytes passed M2-01 path, type, size,
  collision, decompression, and CRC checks.
- **Validated local skill metadata** means M1-03 accepted the staged `SKILL.md` and the name
  matches the catalog record.
- **Untrusted skill content** includes the instructions, scripts, assets, and references. They are
  not executed or treated as higher-priority instructions.

TLS protects the connection to GitHub. A pinned SHA and TLS do not cryptographically prove publisher
identity or content safety. No install command, agent-directory write, receipt, update, removal, or
rollback is part of M2-02.

#### GitHub DOS archive compatibility

The manually downloaded pinned `frontend-design` archive has 543 entries. GitHub encodes 420
ordinary files with ZIP creator system `DOS`, `external_attr=0`, and no directory marker; 97
directories use the DOS directory bit `0x10`; 26 entries carry Unix regular-file metadata. The
initial M2-01 implementation rejected entry 2,
`skills-ca45fc496a3436251f191354091d557da31c15b5/.claude-plugin/marketplace.json`, because its DOS
entry has no Unix mode. The architect approved the narrow fallback documented above as ADR-009. The
complete pinned archive now passes M2-01 validation, exact `frontend-design` source selection,
staging, and M1-03 metadata validation. Regression tests exercise the accepted DOS case and reject
Unix entries without type bits, ambiguous DOS directories, Unix symlinks, and special files.

### M1-04 cache hardening follow-up

M1-04's deferred cache path-component/concurrent-replacement review and recovery-message refinement
remain tracked below. M2-02 does not change the cache and does not expand that follow-up.

Current implementation boundaries through M2-07:

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
  resolver. The installed list parses strict Codex project schema-v1/v2, Codex user schema-v3, and
  Claude schema-v4 and Cursor/Gemini schema-v5 receipts from the OS application-data
  `receipts` directory. It treats missing or empty storage as no recorded installations and rejects
  corrupt, unsupported, duplicated, conflicting, linked, nonregular, or unknown content. Schema-v5
  identities keep Cursor and Gemini distinct from each other and existing agents. Listing
  creates no directories, writes no files, opens no referenced destination paths, and reports current
  file integrity as unchecked. It does not infer ownership from catalog data or agent folders.
- **Archive boundary:** M2-01 applies the ZIP-only validation described above. The validator
  preserves GitHub archive root prefixes and has no network, extraction, install, or
  publisher-authentication behavior.
- **Pinned-source boundary:** M2-02 fetches only a validated resource at its full GitHub commit,
  applies the exact redirect and byte limits above, validates before extraction, selects one exact
  source directory, and returns a validated temporary stage. It does not authenticate publishers,
  execute content, or install into agent folders.
- **Install-plan boundary:** M2-03's `skills install <id> --agent codex --dry-run` uses the existing
  offline `CatalogService::discover()` path, so it does not refresh the catalog. It then fetches
  only the selected resource's exact pinned GitHub archive through M2-02. The validated source
  directory is staged in a private temporary directory, and the planner walks only that selected
  skill. It compares every staged file and directory with the validated archive manifest, enforces
  the archive's count, depth and byte budgets, rechecks each file's size and CRC-32, and rejects
  changed, missing, extra, linked or unsupported objects. CRC-32 here detects accidental or
  post-stage changes; it is not cryptographic authentication.
- **Agent destination boundary:** M2-03 introduced the typed agent target and M3-04 added explicit
  Codex user scope. M3-05 adds Claude Code and M3-06 adds Cursor and Gemini CLI using the same resolver and filesystem engine. The project
  root is the current directory unless `--project` selects another existing directory; the user
  root is the validated OS home and never comes from the working directory. THRN inspects the
  selected agent directory (`.agents`, `.claude`, `.cursor`, or `.gemini`), its `skills` child, and the selected skill
  entry without creating them or following destination symlinks. Inspected directory listings are
  bounded to 16,384 entries each. Non-directory components, symlinks/reparse points, exact target
  paths, and case-insensitive collisions become conflicts. Existing content is externally managed.
  The destination uses validated `skillName`, not the catalog ID or alias. Claude Code follows `.claude/skills`; Cursor uses `.cursor/skills`; Gemini CLI uses `.gemini/skills`. THRN does not interpret `CLAUDE_CONFIG_DIR` as changing Claude skill locations. Cursor and Gemini shared-path compatibility is checked separately before native mutation.
- **Immutable-plan boundary:** `src/install/plan.rs` is a pure builder over a validated staged
  inventory and a read-only destination snapshot. The plan retains canonical resource identity,
  actual skill name, pinned source identity, target selection, directories, files, expected sizes,
  CRC values, conflicts, and destination assumptions. Its data is immutable through the public
  API. A successful plan does not authorize a later write; M2-04 must repeat the inspection and
  compare assumptions immediately before any filesystem change.
- **Dry-run boundary:** successful output is assembled completely before writing to stdout and
  includes the source revision, resolved root, destination, directory list, complete regular-file
  list, sizes and totals, and a no-project-modification statement. Any conflict or failure exits
  nonzero without partial plan output. The stage is dropped and cleaned when planning returns.
  The M2-03 command creates no agent skill directory, receipt, or project file; its temporary stage is
  dropped when planning returns.

### M2-04 internal prepared and committed directory installation

At M2-04, the CLI remained dry-run-only. `src/install/command.rs` has a session path that retains the
validated `StagedSkill`, its staged inventory and digests, and the corresponding immutable plan for
the full commit operation. The session rejects mismatched catalog resource, skill name, source
owner/repository/path/revision, target agent/scope, inventory, file list, or destination identity.
The original M2-04 Codex adapter supported project scope only. M3-04 added explicitly selected
Codex user scope, M3-05 adds Claude Code, and M3-06 adds Cursor/Gemini project and user scope. All use the validated catalog
`skillName`; IDs and aliases are never path components. The adapter does not auto-detect targets or
write other-agent paths.

Before mutation, the engine rechecks the staged inventory and the project and destination snapshot,
including directory identities where the OS exposes them. It opens project directories without
following links, creates missing `.agents` and `skills` components one at a time, and revalidates
their identities and listings. It rejects preexisting target objects and case-insensitive
collisions, even if a caller claims ownership. Linux uses directory-relative `openat` calls with
`O_NOFOLLOW`, and macOS uses the same no-follow directory-relative operations. Windows opens
directories with `FILE_FLAG_OPEN_REPARSE_POINT`, checks reparse attributes and file identities, and
uses a handle-relative final rename. These checks defend against ordinary path replacement and
reparse-point substitution; they do not make arbitrary concurrent same-user filesystem activity a
durable transaction or remove all filesystem-specific race conditions.

The engine creates a random private staging directory inside the already inspected selected-agent
`skills` directory, so staging and final target are on the same filesystem. It copies only
the planned regular files and directories, uses exclusive file creation and bounded 8 KiB reads,
does not carry executable permissions forward, checks CRC-32 and byte counts, and compares a
deterministic SHA-256 digest for each staged source and copied file. SHA-256 here checks copy
integrity; it does not authenticate the publisher or source catalog. It verifies the prepared tree,
rechecks the destination, then renames the complete directory without replacement. Linux requests
`renameat2(RENAME_NOREPLACE)` and macOS requests `renameatx_np(RENAME_EXCL)` through rustix.
Windows uses `SetFileInformationByHandle(FileRenameInfoEx)` with zero flags, a null root handle, and
the fully qualified final path. The source is the opened private stage directory; staging and target
share the validated destination filesystem. Zero flags leave replacement disabled. A missing or
unsupported no-replace primitive is an error; no replacement-capable fallback is used.

Before the final rename, failures remove only files and directories the operation created inside its
private staging directory, then remove newly created parent directories only if they are empty.
Cleanup failures are reported in the structured error; unrelated project content is never
recursively deleted. After a successful rename, the complete skill directory is visible. The rename
does not guarantee disk durability or make receipt creation atomic with the directory. M2-06 adds
a separate durable operation journal and coordinated internal recovery; see [the recovery
protocol](recovery.md). Filesystem and receipt state still cannot be made one power-loss-atomic
transaction across independent storage locations. M2-07 connects this engine to the real install
command while preserving these limits; the implementation is pending independent architectural
review before merge.

## Non-blocking M1-04 follow-up review

The M1-04 cache implementation has two follow-up topics for a later security review; they do not
block offline listing or M2-02 and are not expanded by this milestone:

- **Filesystem hardening:** review path-component and concurrent-replacement races around cache
  directory validation and atomic replacement, and decide whether parent-directory durability
  failures should be surfaced instead of best-effort ignored.
- **Recovery messaging:** refine the cache-fallback diagnostic so it distinguishes corrupt cache
  data from unsafe or inaccessible cache storage, and explains what refresh can repair while
  making clear that failed refreshes preserve the prior cache.

## Non-blocking M2-02 transport review

The source fetcher currently accepts only an initial HTTPS request to `github.com` and one exact
redirect to `codeload.github.com` for the same owner, repository, and full commit. Carry forward an
independent review of the transport host allowlist and proxy-environment behavior, including checks
that diagnostics do not expose credentials or private paths. M2-03 does not broaden or replace this
M2-02 network boundary.

Remaining requirements:

- **M3-01:** safe removal implementation is pending independent architectural review; ADR-013 is a
  proposal. Historical schema-v1 receipts do not retain installation-time destination identity and
  are list-only. New schema-v2 installs bind that identity into the receipt; Windows
  directory-entry durability remains limited.
- **M3-02:** safe update implementation is pending independent architectural review; ADR-014 is a
  proposal. It uses a separate update journal, operation-owned old-directory and old-receipt
  backups, a no-replace receipt transition, and verified forward recovery. It preserves schema-v2
  destination identity and fails closed on schema-v1, modified, or ambiguous content.
- **M3-03:** read-only `skills check` is implemented on local cache/bundled metadata and
  receipt/inventory verification paths; independent architectural review remains required.
- **M3-04:** accepted at `43c5e5639a57c44346b17e91e2245e0591c0a3bb`; ADR-015 is approved.
- **M3-05:** Claude Code project/user lifecycle was accepted at
  `58f8fd962c7837bb5f1ca2fad965fbb5161ad897`; ADR-016 is approved.
- **M3-06:** Cursor and Gemini CLI project/user lifecycle is implemented on `feature/m3-cursor-gemini`; schema-v5 receipts, schema-v4 lifecycle journals, shared-path collision checks, and offline capability inspection use the shared engine. ADR-017 remains proposed pending independent review and five-job CI.
- **Remaining M3:** multi-agent targeting, JSON and noninteractive behavior.

Security-sensitive parsing and writes require negative tests with hostile fixtures, isolated temporary directories, and deterministic network mocks. See the [roadmap](ROADMAP.md) and [repository security policy](../SECURITY.md) for project status.
