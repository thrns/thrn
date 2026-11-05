#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

runner_os="${RUNNER_OS:-}"
if [[ -z "$runner_os" ]]; then
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) runner_os=Windows ;;
    Darwin) runner_os=macOS ;;
    *) runner_os=Linux ;;
  esac
fi

case "$runner_os" in
  Windows) executable="target/release/thrn.exe" ;;
  Linux|macOS) executable="target/release/thrn" ;;
  *) echo "Unsupported runner OS for release smoke test: $runner_os" >&2; exit 1 ;;
esac

[[ -f "$executable" ]] || { echo "Release executable not found: $executable" >&2; exit 1; }

smoke_root="$(mktemp -d)"
trap 'rm -rf "$smoke_root"' EXIT

run_isolated() {
  THRN_CACHE_DIR="$smoke_root/cache" \
  THRN_DATA_DIR="$smoke_root/data" \
    "$executable" "$@"
}

"$executable" --help >/dev/null
"$executable" --version >/dev/null
"$executable" doctor >/dev/null
run_isolated skills list >/dev/null
run_isolated skills list --installed >/dev/null
run_isolated skills list --help >/dev/null

if "$executable" __thrn_invalid_command__ >/dev/null 2>&1; then
  echo 'Invalid command unexpectedly succeeded.' >&2
  exit 1
fi

printf 'Release executable smoke checks passed: %s\n' "$executable"
