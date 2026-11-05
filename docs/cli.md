# CLI contract

This page describes the executable that exists today. The CLI supports offline catalog discovery, an explicit trusted-catalog refresh, read-only local skill validation, and the `doctor` command. Skill downloads and installation, configuration, completions, JSON output, and noninteractive options are not implemented.

## Available commands

| Invocation | Behavior |
|---|---|
| `thrn --help` | Print root usage and the available command list. |
| `thrn --version` | Print the package name and version from Cargo metadata. |
| `thrn doctor` | Print version, reported OS and architecture, and the scope of checks performed. It does not perform filesystem or network checks. |
| `thrn doctor --help` | Print help for the `doctor` subcommand. |
| `thrn skills search [query]` | Search the newest valid local catalog cache, or bundled metadata if no valid cache exists. Omit the query to list all entries. Never uses the network. |
| `thrn skills search --refresh [query]` | Fetch and validate the fixed trusted HTTPS catalog, atomically update the cache, and search the refreshed data. This is the only implemented command that uses the network. |
| `thrn skills info <id>` | Show curated metadata from the newest locally available validated catalog for an exact canonical ID or explicit alias. Never uses the network. |
| `thrn skills validate <path>` | Read and validate the `SKILL.md` in a local skill directory. |

The root parser requires a subcommand. Clap also supplies its normal short-help and short-version aliases.

## Output and exit status

Successful help, version, doctor, search, info, and skill validation output goes to stdout and exits `0`. A search with no matches is an informative successful result. Search and info identify their selected catalog source (`local cache` or `bundled metadata`) and freshness (`fresh`, `stale`, or `unknown`). A successful explicit refresh reports `trusted HTTPS refresh` and `fresh`. Cache problems that permit bundled fallback are reported on stderr; discovery still succeeds. Clap usage errors, including missing required arguments, unknown commands, and invalid options, go to stderr and exit `2`. Invalid lookup identifiers, unknown IDs, skill validation errors, refresh failures, cache replacement failures, and output I/O failures go to stderr and exit `1`. A failed refresh writes no search results to stdout and does not claim that cached or bundled results were refreshed.

## Local skill validation

```sh
thrn skills validate ./path/to/my-skill
```

On success, THRN reports the validated name, description, optional license, compatibility and
allowed-tools values, and the number of additional metadata entries. For example:

```text
Skill validation passed
Name: valid-skill
Description: A minimal synthetic skill used to verify local metadata validation.
License: (not specified)
Compatibility: (not specified)
Allowed tools: (not specified)
Additional metadata entries: 0
```

Failures list actionable frontmatter or file errors on stderr. The validator reads only the regular
`SKILL.md`, has a 256 KiB file ceiling, and applies YAML complexity budgets documented in the
[registry and validation notes](registry.md#local-agent-skills-validation). It does not traverse,
modify, or execute supporting files.

## Platform considerations

`doctor` reports Rust's `std::env::consts::OS` and `ARCH` values. This pair is descriptive metadata, not a full Rust target triple. Doctor text is composed with LF newlines. Clap formats help and diagnostics; consumers and tests should check the meaning, status, and output channel rather than assume byte-for-byte identical help text across platforms or Clap versions.

## Offline discovery and catalog refresh

Default `skills search` and `skills info` are offline. They try the validated cache first, then the embedded catalog. Cached data is fresh through 24 hours after its successful refresh; older cache remains usable and is labeled stale. A missing, corrupt, incompatible, or invalid cache falls back to bundled metadata. The fallback does not trigger a refresh. The cache lives in the OS-standard application cache directory: Linux uses `$XDG_CACHE_HOME/thrn` or `~/.cache/thrn`, macOS uses `~/Library/Caches/im.thrn.thrn`, and Windows uses `%LOCALAPPDATA%/thrn/thrn/cache`. Set `THRN_CACHE_DIR` to an alternate absolute directory when needed; it changes only the local cache path. The file is named `catalog-v1.cache`.

`thrn skills search --refresh [query]` is the only network path. It fetches only `https://raw.githubusercontent.com/thrns/thrn/main/src/registry/builtin-v1.json`. The request requires HTTPS certificate verification, follows no redirects, uses connection/body/overall timeouts, and reads no more than 1 MiB. The response must be UTF-8 and pass the catalog schema validator before it is persisted. A failed request, response validation, or cache commit exits nonzero on stderr. A failed commit leaves the earlier cache intact. The command never fetches skill directories or executes catalog or skill content.

The endpoint is curated and operated by THRN. HTTPS protects the connection to that endpoint; neither the endpoint, catalog inclusion, nor a full Git commit SHA proves the identity or safety of an individual skill publisher. Process-level CLI tests use temporary cache roots and injected environment, and catalog service tests use fake transports, clocks, and cache directories. No test contacts the public endpoint or reads a real user cache. When a future command supports `--no-input`, tests must verify that ambiguous choices fail with an actionable error rather than selecting a target implicitly.
