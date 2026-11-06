# Installation receipts

M2-05 adds a private, local, schema-v1 record for each internal THRN-managed Codex project skill
commit. The receipt store is separate from catalog cache data and lives under the OS application
data directory's `receipts` child. `THRN_DATA_DIR` can select an alternate absolute data directory
for isolated use and tests. Its existing ancestor path is canonicalized without creating missing
components, while the configured data root itself and the `receipts` child are rejected if linked.

## Schema v1

Receipts are UTF-8 JSON objects with strict field validation. Unknown fields, missing fields,
duplicate fields, unsupported schema versions, malformed values, and oversized input fail closed.
Every receipt contains:

- `schemaVersion`: integer `1`.
- `catalogResourceId` and `skillName`: the canonical catalog ID and the distinct installed directory
  name.
- `agent` and `scope`: `codex` and `project`.
- `canonicalProjectRoot`, `projectRootIdentity`, and `destination`: the resolved project root path,
  the root's filesystem volume/object identity, and exactly
  `<canonicalProjectRoot>/.agents/skills/<skillName>`.
- `installedAtUnixSeconds`: installation time in Unix seconds.
- `sourceOwner`, `sourceRepository`, `sourceCommit`, `sourceDirectory`, and `resourceVersion`:
  validated GitHub provenance and the catalog version.
- `directories`: sorted slash-separated relative directories beneath the installed skill root.
- `files`: sorted slash-separated relative paths, expected byte counts, and `sha256:<lowercase hex>`
  digests for every regular file. The installation root itself is implied by the destination.
- `fileCount` and `byteCount`: exact regular-file count and summed byte length.
- `contentDigest`: `sha256:<lowercase hex>` over the canonical inventory encoding below.

Input is capped at 8 MiB, at 4,096 combined inventory entries, at the archive validator's per-file,
total-byte, path-length, and path-depth limits, and at explicit metadata/path lengths. Receipt paths
must pass the same lexical validation as staged archive paths. The parser rejects relative path
traversal, absolute paths, Windows drive/UNC forms, non-ASCII paths unsupported by the current
archive contract, case-insensitive collisions, file/directory overlap, and missing parent
directories. Canonical project path strings also reject control characters and noncanonical
separators or dot components.

## Identity and aggregate digest

The receipt filename is `v1-<64 lowercase hex>.json`. The filename digest is SHA-256 over
`THRN-RECEIPT-IDENTITY-V1\0` followed by three UTF-8 fields in this order: canonical project-root
path, agent ID, canonical catalog resource ID. Each field is encoded as an unsigned 64-bit
big-endian byte length followed by its bytes. This domain separation and length prefixing make the
identity unambiguous. Skill directory names are not identity keys, and aliases are resolved to
canonical catalog IDs before the internal install plan is built.

The aggregate content digest uses SHA-256 over `THRN-CONTENT-INVENTORY-V1\0`, then:

1. An unsigned 64-bit big-endian directory count.
2. For each directory in ascending UTF-8 path-byte order: byte `D`, an unsigned 64-bit big-endian
   path length, then the path bytes.
3. An unsigned 64-bit big-endian file count.
4. For each file in ascending UTF-8 path-byte order: byte `F`, an unsigned 64-bit big-endian path
   length, the path bytes, an unsigned 64-bit big-endian file length, and the raw 32-byte SHA-256
   digest of that file.

Every entry has a type tag, every path has a length, and every file carries its byte length and
digest. The encoding does not depend on JSON key order or ambiguous string concatenation.

## Storage and publication

Listing opens existing storage through the M2-04 no-follow directory-handle layer. It never creates
the data or receipts directory, never fetches the catalog, and never opens a project path. Unknown
files, directories, links/reparse points, malformed receipts, unsupported versions, key mismatches,
duplicate identities, and multiple resource claims for one destination are errors rather than an
empty list.

Receipt creation is available only behind the internal commit/receipt boundary. It reserves the
store with the persistent `.writer.lock` file and an exclusive OS file lock, validates existing
receipts, and rejects a key or destination conflict before the project commit. The lock file is
never removed based on age or PID; a live OS lock excludes competing install and recovery work. The
same lock coordinates the journaled recovery API. After a successful no-replace directory commit,
the writer reopens the committed tree without following links, checks the project and destination
identities, compares the complete directory/file inventory, and rehashes every regular file. It
then serializes within the size limit, creates an unpredictable temporary file exclusively in
receipt storage, writes and flushes the complete JSON, syncs the file, parses it back, and publishes
it with a platform no-replace rename. A pre-existing receipt is never overwritten. A temporary
receipt is recoverable only when its complete strict contents equal the journal's receipt and the
committed destination still verifies. Other pending content is preserved and reported as a conflict.

On Linux and macOS, file data and containing directory entries are synchronized with `sync_all`
before a successful result. On Windows receipt file bytes are flushed before rename, but the current
directory-handle path cannot portably flush directory-entry metadata; entry durability depends on
the filesystem and OS. A directory-sync error after publication is reported as incomplete receipt
persistence even though the final receipt may be visible. Abrupt termination can leave a temporary
receipt or the persistent lock file; OS locks are released by process exit, but recovery still
treats pending receipt data conservatively. Read-only listing never removes journal or receipt state
and does not imply that an unreceipted destination is managed. See [the recovery protocol](recovery.md)
for journal state, retry, and durability behavior.

## Trust and platform path identity

A receipt is a THRN ownership claim and an integrity snapshot. Its hashes detect content differences
when compared with files, but they do not authenticate a publisher, prove authorization to delete,
or establish that a skill is safe. Receipt JSON and all paths it contains remain untrusted input.
Installed listing validates paths structurally but reports `current content integrity: not checked`;
it does not touch referenced project locations. The internal creation path validates the receipt
before opening any project path and verifies the exact committed destination. No M2-05 operation
removes, updates, repairs, or overwrites skill files.

Project roots are keyed by their canonical UTF-8 path string. Receipts also record a filesystem
identity: device and inode on Unix, and volume serial plus file index on Windows. These identities
are platform-specific and may change when a project is copied, restored, or recreated. Path spelling
can differ by case on Windows and common macOS filesystems; destination conflict checks conservatively
compare paths case-insensitively and also compare the stored project identity. Reusing a receipt on a
different machine is not supported as proof that the target directory is the same object.

Receipts do not upgrade manually installed or otherwise unreceipted skills into THRN-owned content.
Corrupt records never authorize removal. Public installation remains disabled pending independent
architectural review of M2-06 recovery and cross-platform tests.
