# Installation receipts

M2-05 introduced private schema-v1 receipts for THRN-managed Codex project skills. Project installs
publish schema-v2 receipts, which add the final destination directory's installation-time
filesystem identity. Explicit user-scope installs publish schema-v3 receipts, which additionally
bind the actual user home and scope-root identity. Claude Code project and user installs publish
schema-v4 receipts with explicit agent, scope, destination layout, canonical root, and root identity.
Existing Codex project schema-v1/v2 and user schema-v3 receipts keep their JSON shapes, deterministic
keys, and digest meanings unchanged. Schema-v1 remains readable for offline listing and
interrupted-install compatibility, but it cannot authorize removal or update. The store is
separate from catalog cache data and lives under the OS application-data directory's `receipts`
child.
`THRN_DATA_DIR` can select an alternate absolute data directory for isolated use and tests. Its
existing ancestor path is canonicalized without creating missing components, while the configured
data root itself and the `receipts` child are rejected if linked.

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

Schema-v1 records remain valid historical claims. They do not contain installation-time destination
identity, so `skills list --installed` can display them but `skills remove` refuses them. THRN never
fills in that missing field from a directory observed during removal, and never rewrites a v1
receipt to claim ownership of its current directory.

Input is capped at 8 MiB, at 4,096 combined inventory entries, at the archive validator's per-file,
total-byte, path-length, and path-depth limits, and at explicit metadata/path lengths. Receipt paths
must pass the same lexical validation as staged archive paths. The parser rejects relative path
traversal, absolute paths, Windows drive/UNC forms, non-ASCII paths unsupported by the current
archive contract, case-insensitive collisions, file/directory overlap, and missing parent
directories. Canonical project path strings also reject control characters and noncanonical
separators or dot components.

## Project schema v2

Schema v2 contains all project schema-v1 fields and requires `installedDirectoryIdentity`, with `volume` and
`object` values captured from the secure handle to the final committed destination. A v2 receipt is
published only after the install journal durably records the same identity. Project installs compare the
current destination identity with this evidence before removal planning; quarantine and cleanup
continue to revalidate the journaled identity at every destructive boundary.

Schema-v2 `contentDigest` uses a distinct domain separator and includes the installation-time volume
and object values before the canonical inventory. Changing or removing the identity therefore
invalidates the digest. Parsers reject v1 records that contain this field, v2 records that omit it,
unknown fields, unsupported versions, and mismatched digests.

## User schema v3

Schema v3 is reserved for explicit Codex user scope. It retains the validated resource ID, distinct
installed skill name, source provenance, complete file and directory inventory, file lengths, and
SHA-256 digests. It requires `scope: "user"`, `canonicalScopeRoot`, `scopeRootIdentity`, and
`installedDirectoryIdentity`. It omits the project-only `canonicalProjectRoot` and
`projectRootIdentity` fields. The destination must be exactly
`<canonicalScopeRoot>/.agents/skills/<skillName>`, where the canonical root is the validated
operating-system home. The scope-root filesystem identity and committed destination identity are
captured through secure directory handles.

The final v3 content digest has its own domain separator. It binds the canonical scope-root path,
scope, scope-root volume/object identity, installed destination volume/object identity, and the
complete inventory. A pre-commit user receipt intent is carried only in the installation journal;
it has a separate intent digest and is never published as an ownership receipt. After no-replace
commit, recovery records the destination identity and transitions to the strict final v3 digest
before receipt publication.

User receipts are keyed using the `THRN-RECEIPT-IDENTITY-V2\0` domain and length-prefixed fields:
scope, canonical scope-root path, agent, and canonical resource ID. Their filenames use
`v2-<64 lowercase hex>.json`; project schema-v1/v2 keys retain the original `v1-` filename and
identity digest byte-for-byte. The scope and actual home are part of the user key, so the same
resource can have independent project and user receipts. Strict parsing rejects project fields in a
user receipt, user fields in a project receipt, scope/schema mismatches, and a user receipt whose
root or destination does not match the selected home. A project receipt is never reinterpreted as
user ownership.

## Claude Code schema v4

Schema v4 is reserved for `agent: "claude-code"` in either explicit project or user scope. It adds
`destinationLayout: "claude-code-default-v1"` and requires exactly one matching root shape:
`canonicalProjectRoot` with `projectRootIdentity`, or `canonicalScopeRoot` with
`scopeRootIdentity`. The destination must be exactly `<root>/.claude/skills/<skillName>`. The
receipt also requires the committed destination's installation-time identity, full GitHub
provenance, and the same complete sorted inventory of files, directories, byte counts, and
individual SHA-256 values used by the existing engine. Host-reserved names such as `synced` and
the `anthropic-skills` namespace are rejected before a Claude receipt can be created or accepted.

Schema-v4 content digests use `THRN-CONTENT-INVENTORY-V4\0`. They bind agent, scope, layout,
canonical scope-root path and identity, committed destination identity, and the canonical inventory.
The separate `THRN-RECEIPT-IDENTITY-V4\0` key binds agent, scope, layout, canonical root and root
identity, then agent and canonical resource ID. Filenames use `v4-<64 lowercase hex>.json`. V4
install intents use a distinct intent digest inside the install journal; a receipt is published
only after the no-replace directory commit captures the final destination identity.

Strict validation never treats an old Codex receipt as a Claude claim. Codex retains historical
schema-v1/v2 project keys and schema-v3 user keys byte-for-byte; Claude v4 has separate keys and
ownership identity. The same resource and skill name therefore has independent receipts for both
agents and both scopes.

## Identity and aggregate digest

The project receipt filename is `v1-<64 lowercase hex>.json`. The filename digest is SHA-256 over
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
digest. The encoding does not depend on JSON key order or ambiguous string concatenation. Schema v2
uses its version-2 domain separator and prefixes this inventory with the recorded destination volume
and object values. User schema v3 uses the separate v3 domain described above; Claude schema v4
binds agent, scope, destination layout, canonical root and root identity, committed destination
identity, and inventory under its own domain.

## Storage and publication

Listing opens existing storage through the M2-04 no-follow directory-handle layer. It never creates
the data or receipts directory, never fetches the catalog, and never opens a skill destination. Unknown
files, directories, links/reparse points, malformed receipts, unsupported versions, key mismatches,
duplicate identities, and multiple resource claims for one destination are errors rather than an
empty list.

Receipt creation is available only behind the internal commit/receipt boundary. It reserves the
store with the persistent `.writer.lock` file and an exclusive OS file lock, validates existing
receipts, and rejects a key or destination conflict before the project commit. The lock file is
never removed based on age or PID; a live OS lock excludes competing install and recovery work. The
same lock coordinates the journaled recovery API. Before commit, the operation journal records the
legacy-shaped receipt intent. After a successful no-replace directory commit, the writer reopens
the committed destination through the secure directory-handle layer, captures its filesystem
identity, and durably records that identity with the upgraded v2 receipt in the next journal
snapshot. It then compares the complete directory/file inventory and rehashes every regular file
before publishing that receipt. The writer creates an unpredictable temporary file exclusively in
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
it does not touch referenced destinations. The internal creation path validates the receipt before
opening the selected scope root and verifies the exact committed destination.

Project and user scope roots are keyed by their canonical UTF-8 path string. Receipts also record a
filesystem identity: device and inode on Unix, and volume serial plus file index on Windows. These identities
are platform-specific and may change when a project is copied, restored, or recreated. Path spelling
can differ by case on Windows and common macOS filesystems; destination conflict checks conservatively
compare paths case-insensitively and also compare the stored scope-root identity. Reusing a receipt on a
different machine is not supported as proof that the target directory is the same object.

Receipts do not upgrade manually installed or otherwise unreceipted skills into THRN-owned content.
Corrupt records never authorize removal or update. Only explicit Codex or Claude Code selection is
supported; other agent adapters are future work. `skills remove` does not need the current catalog: it selects
one exact canonical resource ID in the selected scope and verifies the recorded root identity,
installation-time destination identity, exact destination path, every directory and file, and each
file's byte count and SHA-256 before building a read-only plan. Extra, missing, linked, hard-linked,
replaced, modified, or unsupported objects block removal. User scope does not adopt legacy Codex
locations such as `$CODEX_HOME/skills`.

## Compatibility and migration

Existing Codex project schema-v1 and schema-v2 receipt bytes, keys, and destination identities are
preserved. Listing continues to parse schema-v1, while automatic removal and update refuse because
installation-time directory ownership cannot be established. No receipt is migrated and no
unreceipted directory is adopted. Codex project installs continue to write schema v2 and explicit
Codex user installs write schema v3. Claude Code project and user installs write schema v4. No
agent's receipt keys overlap another agent's key, and no old project receipt is reinterpreted as
Claude ownership. User scope does not reinterpret old project receipts or
legacy Codex directories such as `$CODEX_HOME/skills`.

Project install operation-journal schema v1 remains readable. Its historical receipt intent is
schema v1; an interrupted installation can still recover under its original journal identity and
inventory checks. Existing project receipts are never rewritten. New project operations persist
the identity-bound v2 receipt in the journal before receipt publication and success. User install,
remove, and update operations use separate strict journal schema versions that carry v3 Codex user
or v4 Claude receipts and bind the selected agent and scope root. Recovery retains original project
journal parsing and cannot change an operation's scope.

An interrupted pre-v2 removal journal cannot authorize new destructive recovery because it lacks
installation-time evidence. Nonterminal legacy removal journals are left intact with content
preserved and require independent inspection. Terminal journal cleanup remains verifiable without
deleting project content.

Project and user operations retain the selected agent, scope root, scope-specific receipt, and
transaction identities. All four targets use the same exclusive writer lock and recovery admission.
Recovery refuses to mutate a pending operation for another agent, scope, or root. User operations
never derive their root from the current working directory.

Removal uses the same exclusive writer lock as installation. It journals intent, moves the verified
directory to an operation-private sibling quarantine with a no-replace rename, removes only
receipt-listed verified regular files and empty recorded directories, and retires the ownership
receipt only after the content-removal evidence is durable to the level supported by the platform.
An interrupted removal blocks new install admission and installed listing until recovery finishes.
Read-only `--dry-run` creates no journal and changes neither project nor receipt storage. See the
[recovery protocol](recovery.md) for the state machine and recovery behavior. Installed listing is
offline and read-only; it reports receipt ownership and does not verify current content integrity.
A complete installation success still requires receipt publication and install-journal cleanup. If
an install becomes incomplete after directory commitment, the command returns nonzero and recovery
preserves content whose ownership cannot be proven.

## Receipt-backed update transition

An update retains the same deterministic receipt filename because the selected scope root, agent,
and resource identity do not change. The immutable update plan records the active filename, the old
receipt file's filesystem identity and raw SHA-256, and the required transition mode: back up the old
claim, then publish the new claim with the existing no-replace receipt API. The versioned update
journal stores the full old receipt, its raw-byte digest, the new scope-appropriate receipt contents, the
receipt temp/backup names and identities, and the committed replacement directory identity.

Before renaming the old receipt, the active receipt must still match its exact old file identity,
validated receipt fields, and raw-byte digest. The old receipt is renamed without replacement into
an operation-specific regular file in the same receipt directory and synchronized. The new receipt
is prepared in a private exclusive temporary file, captured in the journal, and published through
`publish_receipt_no_replace`; the publication code itself still validates other active claims and
refuses an occupied deterministic filename. While the old record is backed up and the new record is
not yet durably verified, the operation journal remains pending and installed listing fails closed.
Recovery accepts only the exact old active receipt plus no backup, the exact old backup plus no
active receipt, or the exact new active receipt plus the exact old backup, as permitted by the
journaled state. Two independently valid active records cannot claim one destination.

The old receipt backup is retained until the replacement tree and new receipt both verify and the
old skill backup cleanup reaches its terminal state. Its deletion has a durable cleanup intent and
is allowed only after the recorded file identity and old receipt content verify. The new receipt is
never published by unchecked overwrite, and a matching set of hashes alone never authorizes adopting
a directory. A failed or interrupted transition exits nonzero; success is reported only after the
new receipt verifies and transaction finalization completes. See the [update state machine](recovery.md#receipt-backed-update-state-machine).
