# CLI contract

This page describes the executable that exists today. The CLI currently has one subcommand; discovery, installation, configuration, completions, JSON output, and noninteractive options are not implemented.

## Available commands

| Invocation | Behavior |
|---|---|
| `thrn --help` | Print root usage and the available command list. |
| `thrn --version` | Print the package name and version from Cargo metadata. |
| `thrn doctor` | Print version, reported OS and architecture, and the scope of checks performed. It does not perform filesystem or network checks. |
| `thrn doctor --help` | Print help for the `doctor` subcommand. |

The root parser requires a subcommand. Clap also supplies its normal short-help and short-version aliases.

## Output and exit status

Successful help, version, and doctor output goes to stdout and exits `0`. Clap usage errors, including a missing or unknown command and invalid options, go to stderr and exit `2`. Other command errors are reported to stderr and exit `1`; currently, the implemented operational error is failure to write or flush command output. No JSON document or future categorized error code is part of the present contract.

## Platform considerations

`doctor` reports Rust's `std::env::consts::OS` and `ARCH` values. This pair is descriptive metadata, not a full Rust target triple. Doctor text is composed with LF newlines. Clap formats help and diagnostics; consumers and tests should check the meaning, status, and output channel rather than assume byte-for-byte identical help text across platforms or Clap versions.

## Offline and noninteractive behavior

All implemented commands are offline and prompt-free. Integration tests launch the compiled CLI in unique temporary working directories with the environment cleared; the Windows test helper retains `SystemRoot` when present so the child process can start. No test uses a live network service. Future network tests must use deterministic fixtures or test servers. When a future command supports `--no-input`, tests must verify that ambiguous choices fail with an actionable error rather than selecting a target implicitly.
