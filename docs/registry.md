# Versioned catalog schema

THRN's catalog parser accepts a bounded JSON document with this top-level shape:

```json
{
  "schemaVersion": 1,
  "resources": [
    {
      "id": "uncharted",
      "kind": "skill",
      "displayName": "Uncharted Ideation",
      "skillName": "uncharted-ideation",
      "description": "Structured ideation and evaluation.",
      "version": "1.0.0",
      "aliases": ["idea-lab"],
      "source": {
        "provider": "github",
        "owner": "example-org",
        "repository": "skills",
        "path": "skills/uncharted-ideation",
        "commit": "0123456789abcdef0123456789abcdef01234567"
      }
    }
  ]
}
```

The example is schema documentation, not a verified catalog entry. Its commit value is an
intentionally rejected placeholder.

## Fields and identity

- `schemaVersion` is a required JSON integer and must be exactly `1`.
- `resources` is a required array. Each item requires all fields shown above except `aliases`,
  which may be omitted or supplied as an array.
- `kind` is currently `skill`; other values are rejected.
- `id` is the canonical catalog lookup identifier. `aliases` are additional, explicit lookup
  identifiers. Neither aliases nor IDs imply a skill directory name.
- `skillName` is the actual Agent Skills directory name. It uses the Agent Skills-compatible
  lowercase ASCII grammar and is stored separately from `id`. For example, the catalog identifier
  `uncharted` can point to the directory `uncharted-ideation`.
- Lookup identifiers use 1–64 ASCII letters, digits, and single hyphens, with no leading, trailing,
  or repeated hyphens. Original spelling is preserved. Collision checks use ASCII lowercase keys;
  IDs and aliases that differ only by letter case are ambiguous and rejected. This normalization
  applies only to collision and future exact-lookup keys; the parser does not rewrite supplied IDs.
- `displayName` is nonempty, non-whitespace text of at most 128 UTF-8 bytes. `description` is
  nonempty, non-whitespace text of at most 2,048 UTF-8 bytes. Neither may contain control
  characters. `version` is a 1–64 character ASCII token beginning with an alphanumeric character
  and containing only letters, digits, `.`, `+`, `-`, or `_`.

## Source identity and paths

- `source.provider` must be `github`.
- `source.owner` is 1–39 ASCII letters, digits, or hyphens; it must begin and end with a letter or
  digit and cannot contain consecutive hyphens.
- `source.repository` is 1–100 ASCII letters, digits, dots, hyphens, or underscores; it must
  begin and end with a letter or digit and cannot contain consecutive dots.
- `source.path` is a relative, slash-separated repository path, at most 1,024 UTF-8 bytes and 32
  components. Empty components, `.` and `..`, components over 255 bytes, leading `/`, drive
  prefixes, backslashes, and control characters are rejected. This is lexical catalog validation.
  It does not inspect a checkout or archive and does not guarantee safe extraction on any target
  filesystem; later fetching and installation stages must apply their own path, link, collision,
  and file-type checks.
- `source.commit` must contain exactly 40 hexadecimal characters. All-identical sentinels, the
  common repeated `0123456789abcdef` sample, and repeated `deadbeef` placeholders are rejected.
  The typed value is stored in lowercase. A syntactically valid SHA pins an identifier but does
  not prove that the commit exists or who authored or reviewed it.

## Limits and version policy

The parser accepts at most 1 MiB of input, 256 resources, and 32 aliases per resource. Collection
limits are enforced while JSON arrays are decoded; all remaining field checks run after decoding
within the input byte ceiling. Unknown object fields are rejected. A future schema with new
required or optional fields must use a new `schemaVersion`; version 1 will not silently ignore
fields added by a later schema. Missing `aliases` means an empty alias list. Unsupported versions
fail explicitly.

The reusable parser is `registry::catalog::parse_catalog_json` within the `thrn` binary crate; it
returns validated typed values or `CatalogError`. It performs no network access, source inspection,
or file writes. The bundled catalog is parsed through this validator before its records are used.
`CatalogResolver` builds an in-memory exact lookup index from the validated model and searches
catalog text in stable canonical-ID order. Lookup accepts canonical IDs and explicitly declared
aliases only; the actual `skillName` does not become an implicit install identity.

## Trusted endpoint, offline cache, and discovery

THRN embeds `src/registry/builtin-v1.json` in the executable as its offline fallback. The only
runtime catalog endpoint is the fixed, THRN-operated URL
`https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`. Public CLI
arguments cannot select another URL. `thrn skills list`, `thrn skills search [query]`, and
`thrn skills info <id>` never make network requests. They select the newest locally available
validated catalog: a valid cache first, then the embedded catalog. These commands do not download
or install skill content.

Only `thrn skills search --refresh [query]` contacts the endpoint. The client requires HTTPS and
certificate verification, follows no redirects, sets a 5-second connect timeout, a 15-second
response-body timeout, and a 30-second overall timeout. It rejects unsuccessful status codes and
redirects, reads at most 1 MiB plus one detection byte even without `Content-Length`, checks a
declared length against the received body, rejects invalid UTF-8, then passes the JSON bytes through
`parse_catalog_json`. Only a response that passes that parser can be selected or cached. Refresh
reports success only after the cache replacement succeeds.

The cache is a single file named `catalog-v1.cache` in the directory returned by
`directories::ProjectDirs::from("im", "thrn", "thrn")`, unless `THRN_CACHE_DIR` is set to an
alternate absolute path. This variable changes only the local cache location, not the endpoint:

| Platform | Cache directory |
|---|---|
| Linux | `$XDG_CACHE_HOME/thrn`, or `~/.cache/thrn` when unset |
| macOS | `~/Library/Caches/im.thrn.thrn` |
| Windows | `%LOCALAPPDATA%/thrn/thrn/cache` |

The file contains a versioned THRN header, the exact endpoint, the Unix timestamp of the last
successful refresh, and the catalog JSON that was validated before writing. Cache loads verify the
header and source, bound the file size, reject symlinks and nonregular files, and revalidate the
catalog through `parse_catalog_json`. Missing cache is normal. Corrupt, truncated, incompatible,
wrong-source, or schema-invalid cache data is ignored with a stderr diagnostic and bundled fallback.
A refresh can replace such invalid data only after a new response validates successfully.

The cache refresh threshold is 24 hours. A cache refreshed no more than 24 hours ago is labeled
`fresh`; an older cache is labeled `stale` but remains usable offline. Future-dated or unavailable
cache freshness is `unknown`. Stale data never triggers an automatic request. If fetch, response
validation, or cache replacement fails, refresh exits nonzero and leaves the previous cache
untouched. The cache writer creates a temporary file in the same directory, writes and syncs it,
then atomically replaces the destination where the OS supports it.

The endpoint is curated and operated by THRN. HTTPS authenticates the endpoint connection, while
the catalog identifies a source revision using its commit hash. Neither HTTPS, THRN's inclusion of
a catalog record, nor the commit hash proves the identity or safety of the individual skill
publisher. Skill content remains untrusted and is not fetched or executed by catalog commands.

The current `frontend-design` entry uses the explicit alias `frontend`. The upstream
[`SKILL.md` at the pinned revision](https://github.com/anthropics/skills/blob/ca45fc496a3436251f191354091d557da31c15b5/skills/frontend-design/SKILL.md)
declares the name `frontend-design` and describes distinctive visual design, aesthetic direction,
typography, and avoiding templated defaults. Its metadata does not declare a skill version, so the
catalog uses the explicit convention `unversioned` to mean that no upstream skill version is present
in the curated metadata. This is a catalog convention, not an upstream release number. No license
rights are inferred from catalog inclusion.

The full commit SHA pins the source revision for repeatable identification. It does not guarantee
publisher authenticity or constitute a security endorsement. THRN does not execute instructions
from the referenced `SKILL.md`.

Example commands and output:

```text
$ thrn skills search frontend
Catalog source: bundled metadata; freshness: unknown (no validated cache is available).
Curated skill catalog (schema v1)
frontend-design  Frontend Design
  Guidance for distinctive visual design in new or existing interfaces, including aesthetic direction, typography, and avoiding templated defaults.

$ thrn skills info frontend
Curated catalog information (schema v1)
Catalog source: bundled metadata
Catalog freshness: unknown (no validated cache is available)
Canonical catalog ID: frontend-design
Display name: Frontend Design
Description: Guidance for distinctive visual design in new or existing interfaces, including aesthetic direction, typography, and avoiding templated defaults.
Skill directory name: frontend-design
Catalog version: unversioned
Explicit aliases: frontend
GitHub owner: anthropics
GitHub repository: skills
Source directory: skills/frontend-design
Pinned commit: ca45fc496a3436251f191354091d557da31c15b5
Publisher authenticity: not guaranteed by this commit pin.
```

Malformed lookup identifiers and unknown catalog IDs fail with actionable stderr diagnostics.
Searches with no matches exit successfully and suggest listing the catalog. Catalog refresh errors
write no search results to stdout, report a diagnostic on stderr, and preserve the previous cache.
Test-only data and fixed lexical SHA strings under `tests/fixtures/catalog/` are synthetic and are
not production resources.

## Catalog availability and managed installations

`thrn skills list` is the dedicated catalog listing. It uses the same `CatalogService` selection
and `CatalogResolver` as discovery, displays canonical catalog ID, display name, and description,
and orders entries by canonical ID. Its output identifies the selected source and cache freshness.
An empty catalog is reported as an empty catalog; it does not trigger a refresh. The no-query form
of `thrn skills search` remains supported and displays every catalog entry.

`thrn skills list --installed` has a separate meaning: it checks for THRN-managed installations
only. Receipt storage is planned under the OS application-data directory returned by
`directories::ProjectDirs::from("im", "thrn", "thrn")`, in a `receipts` subdirectory:

| Platform | Application-data directory |
|---|---|
| Linux | `$XDG_DATA_HOME/thrn`, or `~/.local/share/thrn` when unset |
| macOS | `~/Library/Application Support/im.thrn.thrn` |
| Windows | `%APPDATA%/thrn/thrn/data` |

`THRN_DATA_DIR` may select an alternate absolute application-data directory; receipt storage is
then its `receipts` child. This is separate from the catalog cache. A missing or empty receipt
directory prints `No THRN-managed installations are recorded.` and does not inspect Codex, Claude,
Cursor, Gemini, or any other agent directory. It also does not create the application-data or
receipt directories. If receipt storage already contains any content, this milestone reports an
error because its format is not yet recognized. Durable receipt schema and ownership are deferred
to M2-05; catalog entries are never presented as installed records. Installation support has not
been implemented.

## Local Agent Skills validation

`thrn skills validate <path>` checks the local directory's exact `SKILL.md`; it is separate from
catalog validation. The command performs no network access and does not modify the directory. Its
metadata rules follow the [official Agent Skills specification](https://agentskills.io/specification)
and were compared with the [skills-ref reference validator](https://github.com/agentskills/agentskills/tree/main/skills-ref).

The validator requires a regular UTF-8 `SKILL.md` with opening and closing `---` lines and one YAML
mapping. It accepts the standard fields `name` and `description`, plus optional `license`,
`compatibility`, `metadata`, and `allowed-tools`. Unknown top-level fields, duplicate mapping keys,
wrong field types, unsupported tags, YAML anchors/aliases, merge keys, and extra YAML documents are
rejected. `metadata` must be a string-to-string mapping with at most 64 entries. The skill name
must be 1–64 Unicode lowercase alphanumeric characters with single hyphens between parts, and must
exactly match the directory's final name. `description` must be nonempty and at most 1,024 Unicode
characters. A supplied `compatibility` string must contain 1–500 Unicode characters. `license` and
`allowed-tools` must be strings when present; the standard sets no additional length rule for
those fields.

The name is checked as written: THRN does not trim or Unicode-normalize it, and the UTF-8 name
must exactly equal the directory's final name. The reference `skills-ref` implementation applies
NFKC normalization and trimming; the explicit exact-match contract for this command does not.

Resource limits are a 256 KiB total `SKILL.md` size, 16 nested YAML collections, 512 YAML nodes,
2,048 parser events, one YAML document, and zero anchors or aliases. The YAML parser rejects
unsupported tags and merge keys. The Markdown body is preserved and is not format-checked. The
validator does not open scripts, references, assets, or other supporting content.

The existing catalog `SkillName` type remains lowercase ASCII and is not broadened by local
validation. A local skill may therefore pass this command with a Unicode name that the catalog
cannot currently represent. See proposed [ADR-008](DECISIONS.md#architectural-decision-log); no
catalog contract change is implied.

Example failure output:

```text
thrn: skill validation failed
  - frontmatter field `description` must be no more than 1024 characters (found 1025)
```

A successful validation establishes only that the inspected metadata follows these checks. It does
not verify publisher identity, source authenticity, supporting-file safety, or the behavior of
arbitrary scripts. This local metadata validator is not M2 archive extraction or installation
security.
