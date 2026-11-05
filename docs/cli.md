# CLI contract

This page describes the executable that exists today. The CLI supports offline catalog discovery, read-only local skill validation, and the `doctor` command. Skill downloads and installation, configuration, completions, JSON output, and noninteractive options are not implemented.

## Available commands

| Invocation | Behavior |
|---|---|
| `thrn --help` | Print root usage and the available command list. |
| `thrn --version` | Print the package name and version from Cargo metadata. |
| `thrn doctor` | Print version, reported OS and architecture, and the scope of checks performed. It does not perform filesystem or network checks. |
| `thrn doctor --help` | Print help for the `doctor` subcommand. |
| `thrn skills search [query]` | Search the compile-time bundled catalog by ID, alias, skill name, display name, or description. Omit the query to list all entries. |
| `thrn skills info <id>` | Show curated metadata for an exact canonical ID or explicit alias. |
| `thrn skills validate <path>` | Read and validate the `SKILL.md` in a local skill directory. |

The root parser requires a subcommand. Clap also supplies its normal short-help and short-version aliases.

## Output and exit status

Successful help, version, doctor, search, info, and skill validation output goes to stdout and exits `0`. A search with no matches is an informative successful result. Clap usage errors, including missing required arguments, unknown commands, and invalid options, go to stderr and exit `2`. Invalid lookup identifiers, unknown IDs, skill validation errors, filesystem errors, and output I/O failures go to stderr and exit `1`. Failed validation writes no success or partial metadata to stdout.

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

## Offline and noninteractive behavior

All implemented commands are offline and prompt-free. Skill discovery reads a static catalog embedded in the executable; catalog updates require updated metadata to ship with the binary. M1-04 covers runtime catalog fetching and caching. The discovery commands do not retrieve or install skill content and do not establish publisher authenticity. Integration tests launch the compiled CLI in unique temporary working directories with the environment cleared; the Windows test helper retains `SystemRoot` when present so the child process can start. No test uses a live network service. Future network tests must use deterministic fixtures or test servers. When a future command supports `--no-input`, tests must verify that ambiguous choices fail with an actionable error rather than selecting a target implicitly.
