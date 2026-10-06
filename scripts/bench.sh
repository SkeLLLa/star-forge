#!/usr/bin/env bash
# Compare `stfg` (cached by the daemon) with running the same work directly, the way a
# starship `[custom.*]` module would. Uses a throwaway config and runtime dir, so it neither
# touches nor talks to your own daemon. Run from a git checkout (the git badges read it).
#
#   mise run bench                 # all cases
#   SLOW_SECS=1 mise run bench     # slower stand-in for a network call (default 0.3)
#
# Results are printed and written as Markdown to <cargo target dir>/bench/results.md.
set -euo pipefail

slow_secs=${SLOW_SECS:-0.3}
[[ $slow_secs =~ ^[0-9]+([.][0-9]+)?$ ]] || { echo "SLOW_SECS must be a number of seconds" >&2; exit 1; }
git rev-parse --git-dir >/dev/null 2>&1 || { echo "run inside a git checkout" >&2; exit 1; }
for tool in hyperfine jq; do
  command -v "$tool" >/dev/null || { echo "$tool not found; run \`mise install\`" >&2; exit 1; }
done

# The daemon runs git without optional locks; do the same so bare `git status` neither does
# extra work nor rewrites .git/index (which would invalidate the cached git_counts value).
export GIT_OPTIONAL_LOCKS=0
unset STAR_FORGE_TIMEOUT_MS

cargo build --release --bins --quiet
target_dir=$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)
bin="$target_dir/release"
stfg="$bin/stfg"
out="$target_dir/bench"
mkdir -p "$out"

tmp=$(mktemp -d)
cleanup() {
  "$bin/stfgd" stop >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT
export XDG_RUNTIME_DIR="$tmp/run" STAR_FORGE_CONFIG="$tmp/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"

# The slow fetch must finish inside its timeout (daemon default 2s), so derive it from SLOW_SECS.
# idle_exit is a backstop if the final stop fails; it must outlast the bare halves of the slow
# cases (about 15 runs of SLOW_SECS with no stfg request).
timeout_ms=$(awk -v s="$slow_secs" 'BEGIN { printf "%d", s * 1000 + 2000 }')
idle_exit_s=$(awk -v s="$slow_secs" 'BEGIN { printf "%d", s * 20 + 120 }')
slow_cmd="sleep $slow_secs; echo ok"
cat >"$STAR_FORGE_CONFIG" <<EOF
[daemon]
idle_exit = "${idle_exit_s}s"

[badge.git_branch]
type = "builtin"
name = "git_branch"

[badge.git_modified]
type = "builtin"
name = "git_counts"
field = "modified"

[badge.slow]
type = "command"
command = "$slow_cmd"
interval = "10m"
timeout = "${timeout_ms}ms"

[groups.prompt]
badges = ["git_branch", "git_modified", "slow"]
EOF

# Wait until every badge serves the value the bare command gives; an empty line would mean the
# timings measure stfg's fast failure path, not a cache hit. git_counts renders 0 as empty and
# git_branch is empty on a detached HEAD, exactly like the bare commands below.
expected_branch=$(git branch --show-current)
expected_modified=$(git status --porcelain=v2 |
  awk '($1 == "1" || $1 == "2") && substr($2, 2, 1) != "." { n++ } END { if (n) print n }')
# A group joins only its non-empty values.
values=()
for v in "$expected_branch" "$expected_modified" ok; do
  [[ -n $v ]] && values+=("$v")
done
expected_prompt="${values[*]}"
warm() {
  [[ $("$stfg" prompt) == "$expected_prompt" &&
    $("$stfg" git_branch) == "$expected_branch" &&
    $("$stfg" git_modified) == "$expected_modified" ]]
}
for _ in $(seq $((timeout_ms / 100 + 50))); do
  warm && break
  sleep 0.1
done
warm || {
  echo "daemon serves '$("$stfg" prompt)', expected '$expected_prompt'" >&2
  exit 1
}
printf 'cached values: %s\n' "$expected_prompt"

results="$out/results.md"
: >"$results"

bench() {
  local title=$1
  shift
  printf '\n### %s\n\n' "$title" | tee -a "$results"
  hyperfine -N --warmup 5 --export-markdown "$out/case.md" "$@"
  cat "$out/case.md" >>"$results"
  # hyperfine discards output; make sure the rows above were cache hits throughout.
  warm || { echo "daemon stopped serving cached values during the run" >&2; exit 1; }
}

bench "Git branch" \
  -n "stfg git_branch" "'$stfg' git_branch" \
  -n "git branch --show-current" "git branch --show-current"

bench "Git status counts" \
  -n "stfg git_modified" "'$stfg' git_modified" \
  -n "git status --porcelain=v2 --branch" "git status --porcelain=v2 --branch"

bench "Slow command (${slow_secs}s, stands in for a network call)" \
  -n "stfg slow" "'$stfg' slow" \
  -n "sh -c '$slow_cmd'" "sh -c '$slow_cmd'"

bench "Whole prompt: three badges" \
  -n "stfg prompt (one group)" "'$stfg' prompt" \
  -n "bare commands" \
  "sh -c 'git branch --show-current; git status --porcelain=v2 --branch; $slow_cmd'"

rm -f "$out/case.md"
printf '\nMarkdown results: %s\n' "$results"
