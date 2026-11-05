#!/usr/bin/env bash
# Deterministic integration coverage for finish-task.sh with isolated Git repos
# and a local bare remote. GitHub CLI behavior is mocked; no network is used.
set -euo pipefail

source_root="$(cd "$(dirname "$BASH_SOURCE")/../.." && pwd)"
real_git="$(command -v git)"
tmp_root="$(mktemp -d /tmp/thrn-finish-task.XXXXXX)"
trap 'rm -rf "$tmp_root"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_eq() {
  local actual="$1" expected="$2" label="$3"
  [[ "$actual" == "$expected" ]] || fail "$label (expected '$expected', got '$actual')"
}
assert_contains() {
  local haystack="$1" needle="$2" label="$3"
  [[ "$haystack" == *"$needle"* ]] || fail "$label (missing '$needle')"
}
count_events() {
  local event="$1" count
  count="$(grep -c "^$event\$" "$events_file" 2>/dev/null || true)"
  if [[ -z "$count" ]]; then count=0; fi
  printf '%s' "$count"
}

new_fixture() {
  local name="$1"
  fixture="$tmp_root/$name"
  remote_dir="$tmp_root/$name-remote.git"
  bin_dir="$fixture/test-bin"
  events_file="$tmp_root/$name-events"
  run_out="$tmp_root/$name-run.out"
  pr_file="$tmp_root/$name-pr-exists"
  mkdir -p "$fixture/scripts" "$fixture/.githooks" "$fixture/.github" "$bin_dir"
  cp "$source_root/scripts/finish-task.sh" "$fixture/scripts/finish-task.sh"
  cp "$source_root/.githooks/pre-push" "$fixture/.githooks/pre-push"
  cp "$source_root/.gitignore" "$fixture/.gitignore"
  cat > "$fixture/scripts/verify.sh" <<'VERIFY'
#!/usr/bin/env bash
set -euo pipefail
printf 'verify\n' >> "$THRN_TEST_EVENTS"
if [[ "$THRN_TEST_FAIL_VERIFY" == "1" ]]; then
  echo 'injected verification failure' >&2
  exit 23
fi
echo 'fixture verification passed'
VERIFY
  cat > "$bin_dir/git" <<'GIT'
#!/usr/bin/env bash
set -euo pipefail
if (($# == 0)); then exec "$THRN_TEST_REAL_GIT"; fi
case "$1" in
  push)
    shift
    [[ "$1" == "-u" ]] && shift
    [[ "$1" == "origin" ]] || { echo 'unexpected push remote' >&2; exit 90; }
    shift
    printf 'push\n' >> "$THRN_TEST_EVENTS"
    GIT_CONFIG_COUNT=1 \
      GIT_CONFIG_KEY_0="url.$THRN_TEST_REMOTE.insteadOf" \
      GIT_CONFIG_VALUE_0="https://github.com/thrns/thrn.git" \
      "$THRN_TEST_REAL_GIT" push -u origin "$@"
    ;;
  ls-remote)
    shift
    [[ "$1" == "--heads" ]] && shift
    [[ "$1" == "origin" ]] || { echo 'unexpected ls-remote origin' >&2; exit 91; }
    shift
    exec "$THRN_TEST_REAL_GIT" ls-remote --heads "$THRN_TEST_REMOTE" "$@"
    ;;
  *)
    exec "$THRN_TEST_REAL_GIT" "$@"
    ;;
esac
GIT
  cat > "$bin_dir/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == "auth" && "$2" == "status" ]]; then exit 0; fi
if [[ "$1" == "pr" && "$2" == "list" ]]; then
  printf 'gh-list\n' >> "$THRN_TEST_EVENTS"
  [[ "$*" == *'--state all'* ]] || { echo 'PR lookup omitted all states' >&2; exit 95; }
  [[ "$*" == *'--json url,state,mergedAt,isDraft'* ]] || { echo 'PR lookup omitted state fields' >&2; exit 96; }
  if [[ -f "$THRN_TEST_PR_FILE" ]]; then
    printf 'https://github.com/thrns/thrn/pull/4242|%s|%s|%s\n' \
      "$THRN_TEST_PR_STATE" "$THRN_TEST_PR_MERGED_AT" "$THRN_TEST_PR_IS_DRAFT"
  fi
  exit 0
fi
if [[ "$1" == "pr" && "$2" == "create" ]]; then
  printf 'gh-create\n' >> "$THRN_TEST_EVENTS"
  [[ "$*" == *'--draft'* ]] || { echo 'new pull request was not created as draft' >&2; exit 97; }
  body_file=''
  while (($#)); do
    if [[ "$1" == "--body-file" ]]; then
      shift
      if (($#)); then body_file="$1"; fi
    fi
    shift
  done
  [[ -n "$body_file" && -f "$body_file" ]] || { echo 'missing generated PR body' >&2; exit 92; }
  grep -q 'Verified commit:' "$body_file" || { echo 'PR body missing SHA evidence' >&2; exit 93; }
  if [[ "$GH_CREATE_FAIL" == "1" ]]; then
    echo 'injected PR creation failure' >&2
    exit 24
  fi
  : > "$THRN_TEST_PR_FILE"
  printf 'https://github.com/thrns/thrn/pull/4242\n'
  exit 0
fi
echo "unexpected gh invocation: $*" >&2
exit 94
GH
  chmod +x "$bin_dir/git" "$bin_dir/gh" "$fixture/scripts/verify.sh" "$fixture/.githooks/pre-push"
  "$real_git" init -q --initial-branch=main "$fixture"
  "$real_git" -C "$fixture" config user.name 'THRN Test'
  "$real_git" -C "$fixture" config user.email 'thrn-test@example.invalid'
  "$real_git" -C "$fixture" config commit.gpgsign false
  printf 'baseline\n' > "$fixture/README.md"
  "$real_git" -C "$fixture" add .gitignore README.md scripts .githooks .github test-bin
  "$real_git" -C "$fixture" commit -qm 'test: baseline'
  "$real_git" init -q --bare --initial-branch=main "$remote_dir"
  "$real_git" -C "$fixture" remote add origin https://github.com/thrns/thrn.git
  "$real_git" -C "$fixture" push "$remote_dir" main >/dev/null
  "$real_git" -C "$fixture" checkout -qb chore/recovery-test
  "$real_git" -C "$fixture" push "$remote_dir" chore/recovery-test >/dev/null
  base_sha="$("$real_git" -C "$fixture" rev-parse HEAD)"
}

run_finish() {
  local verify_fail="$1" create_fail="$2" pr_state="${3:-OPEN}" pr_is_draft="${4:-true}" pr_merged_at="${5:-}"
  (
    cd "$fixture"
    PATH="$bin_dir:$PATH" \
    THRN_TEST_REAL_GIT="$real_git" \
    THRN_TEST_REMOTE="$remote_dir" \
    THRN_TEST_EVENTS="$events_file" \
    THRN_TEST_PR_FILE="$pr_file" \
    THRN_TEST_PR_STATE="$pr_state" \
    THRN_TEST_PR_IS_DRAFT="$pr_is_draft" \
    THRN_TEST_PR_MERGED_AT="$pr_merged_at" \
    THRN_TEST_FAIL_VERIFY="$verify_fail" \
    GH_CREATE_FAIL="$create_fail" \
      bash scripts/finish-task.sh 'fix: make pull request delivery recoverable' 'chore: establish THRN development infrastructure'
  )
}

remote_branch_sha() {
  "$real_git" ls-remote --heads "$remote_dir" refs/heads/chore/recovery-test | awk 'NR == 1 { print $1 }'
}

echo 'case: successful verification and delivery'
new_fixture success
printf 'corrective change\n' >> "$fixture/README.md"
run_finish 0 0 > "$run_out" 2>&1 || fail 'successful delivery returned nonzero'
head_sha="$("$real_git" -C "$fixture" rev-parse HEAD)"
assert_eq "$(remote_branch_sha)" "$head_sha" 'successful delivery pushed the commit'
assert_eq "$(count_events push)" '1' 'successful delivery pushed once'
assert_eq "$(count_events gh-create)" '1' 'successful delivery created one PR'
assert_eq "$("$real_git" -C "$fixture" rev-list --count HEAD)" '2' 'successful delivery created one non-empty commit'
[[ "$(count_events verify)" -ge 2 ]] || fail 'verification and pre-push hook must both run'

echo 'case: verification failure before commit or push'
new_fixture verify-failure
printf 'unverified change\n' >> "$fixture/README.md"
if run_finish 1 0 > "$run_out" 2>&1; then fail 'verification failure returned zero'; fi
assert_eq "$("$real_git" -C "$fixture" rev-parse HEAD)" "$base_sha" 'failed verification made no commit'
assert_eq "$(count_events push)" '0' 'failed verification made no push'
assert_eq "$(count_events gh-create)" '0' 'failed verification made no PR'

echo 'case: push succeeds but PR creation fails'
new_fixture create-failure
printf 'verified change\n' >> "$fixture/README.md"
if run_finish 0 1 > "$run_out" 2>&1; then fail 'injected PR creation failure returned zero'; fi
pushed_sha="$("$real_git" -C "$fixture" rev-parse HEAD)"
assert_eq "$(remote_branch_sha)" "$pushed_sha" 'push completed before PR failure'
assert_eq "$(count_events push)" '1' 'first attempt pushed once'
assert_eq "$(count_events gh-create)" '1' 'first attempt tried PR creation once'

echo 'case: retry PR creation after push, without another code change'
run_finish 0 0 > "$tmp_root/create-failure-retry.out" 2>&1 || fail 'PR retry returned nonzero'
assert_eq "$("$real_git" -C "$fixture" rev-parse HEAD)" "$pushed_sha" 'PR retry made no commit'
assert_eq "$(remote_branch_sha)" "$pushed_sha" 'PR retry kept same pushed commit'
assert_eq "$(count_events push)" '1' 'PR retry did not push again'
assert_eq "$(count_events gh-create)" '2' 'PR retry retried creation'
assert_contains "$(cat "$tmp_root/create-failure-retry.out")" 'No new changes to commit' 'retry path is reported'

echo 'case: repeated execution reuses existing PR'
run_finish 0 0 > "$tmp_root/create-failure-repeat.out" 2>&1 || fail 'repeat execution returned nonzero'
assert_eq "$(count_events push)" '1' 'repeat execution did not push'
assert_eq "$(count_events gh-create)" '2' 'repeat execution did not create duplicate PR'
assert_eq "$(count_events gh-list)" '3' 'each delivery queried for an existing PR'
assert_contains "$(cat "$tmp_root/create-failure-repeat.out")" 'Reusing existing draft pull request' 'open draft PR reuse is reported'
[[ "$(count_events verify)" -ge 4 ]] || fail 'clean-tree retries must still verify'

echo 'case: closed unmerged PR fails without creating a duplicate'
new_fixture closed-pr
: > "$pr_file"
if run_finish 0 0 CLOSED false > "$run_out" 2>&1; then fail 'closed unmerged PR returned success'; fi
assert_contains "$(cat "$run_out")" 'Existing pull request is closed and unmerged' 'closed PR failure is explained'
assert_contains "$(cat "$run_out")" 'Reopen it manually or start a new task branch' 'closed PR recovery is actionable'
assert_eq "$(count_events gh-create)" '0' 'closed PR did not create a duplicate'
assert_eq "$(count_events push)" '0' 'closed PR retry did not push again'
assert_eq "$(remote_branch_sha)" "$base_sha" 'closed PR failure preserved the verified remote SHA'

echo 'case: refusing a direct push to main'
new_fixture main-refusal
"$real_git" -C "$fixture" checkout -q main
if run_finish 0 0 > "$fixture/main.out" 2>&1; then fail 'main branch delivery returned zero'; fi
assert_contains "$(cat "$fixture/main.out")" 'Direct pushes to main are forbidden' 'main branch is rejected'
assert_eq "$(count_events push)" '0' 'main branch was not pushed'
zero_sha='0000000000000000000000000000000000000000'
if (
  cd "$fixture"
  printf 'refs/heads/chore/recovery-test %s refs/heads/main %s\n' "$base_sha" "$zero_sha" |
    THRN_TEST_REAL_GIT="$real_git" PATH="$bin_dir:$PATH" bash .githooks/pre-push
) > "$tmp_root/main-refusal-hook.out" 2>&1; then
  fail 'pre-push hook accepted a main destination'
fi
assert_contains "$(cat "$tmp_root/main-refusal-hook.out")" 'Direct pushes to main are blocked' 'pre-push hook rejects main destination'

echo 'All finish-task automation tests passed.'
