#!/usr/bin/env bash
# Local release gate. No --fix / --allow-dirty flags: fails closed.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
command -v cargo >/dev/null || { echo 'Rust/Cargo missing; see START_HERE.md' >&2; exit 1; }
printf '==> rustfmt\n'
cargo fmt --all -- --check
printf '==> clippy\n'
cargo clippy --locked --all-targets --all-features -- -D warnings
printf '==> tests\n'
cargo test --locked --all-targets --all-features
printf '==> release build\n'
cargo build --locked --release
printf '==> working-tree whitespace check\n'
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then git diff --check; fi
printf '\nAll local gates passed. Remote CI and acceptance review still required.\n'
