#!/usr/bin/env bash
# Phase 2 head-to-head: chitra vs code-review-graph (RISK-1 gate).
# Reproducible; prints a table. Honest — reports what differs, not best-case.
#
# Usage: benchmark/run.sh <target-repo> [base-ref]
#   target-repo : a git repo to graph + review (default ../code-review-graph)
#   base-ref    : git ref to diff against         (default HEAD~3)
set -euo pipefail

REPO="${1:-../code-review-graph}"
BASE_REL="${2:-HEAD~3}"
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/chitra"
[ -x "$BIN" ] || BIN="$BIN.exe"
DB="$(mktemp -u).db"
BASE="$(git -C "$REPO" rev-parse "$BASE_REL")"

echo "repo=$REPO  base=$BASE_REL ($BASE)"
echo

# --- chitra ---
t0=$(date +%s.%N); "$BIN" build "$REPO" --db "$DB" >/dev/null; t1=$(date +%s.%N)
CH_BUILD=$(echo "$t1 - $t0" | bc)
"$BIN" detect-changes --base "$BASE" --root "$REPO" --db "$DB" > "$DB.dc.json"
CH_DC_TOK=$("$BIN" estimate "$DB.dc.json")
# naive baseline: whole changed files
mapfile -t FILES < <(git -C "$REPO" diff --name-only "$BASE" | sed "s#^#$REPO/#")
NAIVE_TOK=$("$BIN" estimate "${FILES[@]}" 2>/dev/null || echo 0)

# --- code-review-graph (optional; needs `uv` + the repo's own env) ---
CRG_BUILD="n/a"; CRG_DC_TOK="n/a"
if command -v uv >/dev/null 2>&1 && [ -f "$REPO/pyproject.toml" ]; then
  ( cd "$REPO" && uv sync >/dev/null 2>&1 ) || true
  s0=$(date +%s.%N)
  ( cd "$REPO" && uv run python -m code_review_graph build >/dev/null 2>&1 ) && {
    s1=$(date +%s.%N); CRG_BUILD=$(echo "$s1 - $s0" | bc)
    ( cd "$REPO" && uv run python -m code_review_graph detect-changes --base "$BASE" ) \
      > "$DB.crg.txt" 2>/dev/null && CRG_DC_TOK=$("$BIN" estimate "$DB.crg.txt")
  } || true
fi

printf '%-28s %-18s %-18s\n' "metric" "chitra" "code-review-graph"
printf '%-28s %-18s %-18s\n' "install" "static binary" "uv sync + Python"
printf '%-28s %-18s %-18s\n' "full build (s)" "$CH_BUILD" "$CRG_BUILD"
printf '%-28s %-18s %-18s\n' "detect-changes tokens" "$CH_DC_TOK" "$CRG_DC_TOK"
printf '%-28s %-18s %-18s\n' "naive full-files tokens" "$NAIVE_TOK" "$NAIVE_TOK"
echo
echo "token reduction vs naive: $(echo "scale=1; $NAIVE_TOK / $CH_DC_TOK" | bc)x"
