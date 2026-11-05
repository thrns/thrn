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
intentionally rejected placeholder. No production catalog is added by M1-01.

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
file writes, alias resolution, or installation. M1-02 owns catalog lookup and alias resolution.
Test-only data and fixed lexical SHA strings live under `tests/fixtures/catalog/` and are not
production resources.
