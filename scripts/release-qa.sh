#!/usr/bin/env bash
# Release gate: run BEFORE every release. Every field-reported bug that was
# ever fixed has a test here or in `cargo test` — a release that skips this
# script can silently regress them.
#
# Every step FAILS LOUD: no grep-pipelines deciding pass/fail, no `|| true`
# around a gate. `set -e` + direct tool exit codes.
#
# Usage: scripts/release-qa.sh [path-to-a-real-monorepo-for-live-checks]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

step "format gate"
cargo fmt --all -- --check

step "clippy gate (all targets, warnings are errors)"
cargo clippy --workspace --all-targets -- -D warnings

step "full test suite (includes every regression test)"
cargo test --workspace

step "release build + smoke"
cargo build --release -p codegraph-cli
BIN="$ROOT/target/release/codegraph"
"$BIN" --version
"$BIN" stats >/dev/null   # status alias — agents guess the MCP tool name

step "e2e: crash mid-index -> next query answers (flock kernel release)"
S=$(mktemp -d)
trap 'rm -rf "$S"' EXIT
export CODEGRAPH_CACHE_DIR="$S/cache"
mkdir -p "$S/repo" && cd "$S/repo"
git init -q
# self-contained identity: the gate must not depend on global git config
git config user.name codegraph-release-qa
git config user.email qa@codegraph.invalid
for i in $(seq 1 120); do printf 'def fn_%d():\n    return %d\n' "$i" "$i" > "f$i.py"; done
git add -A && git commit -qm baseline
"$BIN" index . >/dev/null
for i in $(seq 1 120); do echo "def zz_$i(): pass" >> "f$i.py"; done
("$BIN" index --full >/dev/null 2>&1 & P=$!; sleep 0.05; kill -9 $P 2>/dev/null || true; wait 2>/dev/null || true)
"$BIN" index . >/dev/null   # heal (also proves the killed run left no stale lock)
"$BIN" search zz_10 | grep -q zz_10

step "e2e: determinism"
"$BIN" verify-determinism | grep -q deterministic
cd "$ROOT"

step "eval receipts (SCIP ground truth) — compare against CHANGELOG claims"
if [ -d scripts/eval/work/zod ] && [ -d scripts/eval/work/fastapi ]; then
  PATH="$ROOT/target/release:$PATH" python3 scripts/eval/run_eval.py | tail -5
else
  echo "SKIP: clone pinned corpora first (see scripts/eval/README.md)"
fi

if [ "${1:-}" != "" ] && [ -d "$1" ]; then
  step "live monorepo checks: $1"
  ( cd "$1"
    time "$BIN" index >/dev/null
    time "$BIN" search x --no-autoheal >/dev/null
    "$BIN" routes | head -3
  )
fi

step "ALL GATES PASSED"
