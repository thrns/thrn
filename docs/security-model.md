# Security model

## Current exposure

THRN is under development and has no supported installer release. The current binary parses arguments, prints help/version, and reports static build metadata through `doctor`. It does not fetch registry data or skills, read credentials, write installation files, or execute subprocesses. Therefore, archive extraction, target ownership, installation staging, receipts, and rollback protections do not exist yet; their absence is not a security guarantee for an installer.

## Boundaries required before installation

Registry metadata, GitHub responses, archives, skill files, and instructions inside `SKILL.md` must be treated as untrusted input. A skill is content for a compatible agent, not an instruction source for THRN. Installing or inspecting one must never implicitly invoke scripts, hooks, shells, package managers, or other retrieved code.

Before M2 installation work is accepted, source and filesystem handling must:

- Use the curated HTTPS registry and public GitHub sources at exact, full commit hashes. Validate the source origin and immutable revision; do not follow untrusted redirects. A checksum from the same registry can detect accidental changes but does not prove publisher identity if that registry is compromised.
- Bound network time, response size, archive size, expanded bytes, file count, per-file size, and path depth. Reject malformed metadata, absolute or traversal paths, drive/UNC prefixes, unsupported file types, case-fold collisions, and symlinks or hardlinks that escape the staging or target root.
- Validate Agent Skills frontmatter and the skill directory/name relationship before installation, while preserving legitimate supporting files.
- Treat an existing unreceipted target as externally managed. Never overwrite or recursively remove unrelated or user-modified paths. Require explicit scope and conflict choices; project scope is the default, user scope must be selected, and ambiguous noninteractive agent selection must fail.
- Stage complete content near each destination and use atomic rename where the filesystem supports it. Multiple destinations are a recoverable sequence, not one cross-target atomic transaction. Write durable ownership receipts only after successful commits; preserve recovery state and report incomplete rollback.
- Avoid logging credentials, tokens, environment secrets, private skill content, or unnecessary personal paths. The v1 public-source workflow should not require a GitHub token. Any future credential support needs an explicit design and redaction tests.
- Make supply-chain claims proportionate to evidence. Pinned commits identify source revisions, not author identity. Stronger publisher authenticity requires a separately reviewed signing or attestation design; do not imply that TLS or a same-origin digest provides it.

## Milestone status

These are requirements, not implemented safeguards:

- **M1:** versioned catalog validation, explicit ID/alias resolution, full-commit validation, and standards-aware skill metadata validation.
- **M2:** constrained GitHub fetching, hostile archive/path validation, destination checks, ownership receipts, staged installation, and rollback/recovery.
- **M3:** safe user-scope support and receipt-backed update/remove behavior that detects modified content and reports partial failure.

Security-sensitive parsing and writes require negative tests with hostile fixtures, isolated temporary directories, and deterministic network mocks. See the [roadmap](ROADMAP.md) and [repository security policy](../SECURITY.md) for project status.
