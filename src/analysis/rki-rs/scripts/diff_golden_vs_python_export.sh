#!/usr/bin/env bash
# Compare NDJSON wire events (`WireEvent` one JSON object per line) to the Rust golden corpus.
#
# Usage:
#   bash scripts/diff_golden_vs_python_export.sh [PYTHON_EXPORT.jsonl]
#
# If the argument is omitted: use tests/golden/python_export.jsonl when present, otherwise
# tests/golden/python_export.sample.jsonl (canonical concat of minimal_turn + more_events +
# extra_variants + session_shutdown).
#
# Lines starting with # and blank lines are ignored on the Python side. Each JSON line is
# canonicalized with jq (-S sort keys, -c compact) so key order differences do not false-fail.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GOLDEN_DIR="$ROOT/tests/golden"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required for wire NDJSON diff (brew install jq / apt install jq)" >&2
  exit 2
fi

normalize_ndjson() {
  local in_path="$1"
  local out_path="$2"
  : >"$out_path"
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line//$'\r'/}"
    [[ -z "${line// }" ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    echo "$line" | jq -e -c -S . >>"$out_path" || {
      echo "error: invalid JSON line in $in_path: ${line:0:120}" >&2
      return 1
    }
  done <"$in_path"
}

build_rust_reference() {
  local out="$1"
  : >"$out"
  for name in minimal_turn.jsonl more_events.jsonl extra_variants.jsonl session_shutdown.jsonl; do
    local f="$GOLDEN_DIR/$name"
    if [[ ! -f "$f" ]]; then
      echo "error: missing golden fixture $f" >&2
      return 1
    fi
    cat "$f" >>"$out"
  done
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REF_RAW="$TMP/rust_concat.jsonl"
REF_NORM="$TMP/rust.norm.txt"
PY_RAW="$TMP/python_export.jsonl"
PY_NORM="$TMP/python.norm.txt"

build_rust_reference "$REF_RAW"
normalize_ndjson "$REF_RAW" "$REF_NORM"

if [[ "${1-}" != "" ]]; then
  PY_SRC="$1"
else
  if [[ -f "$GOLDEN_DIR/python_export.jsonl" ]]; then
    PY_SRC="$GOLDEN_DIR/python_export.jsonl"
  else
    PY_SRC="$GOLDEN_DIR/python_export.sample.jsonl"
  fi
fi

if [[ ! -f "$PY_SRC" ]]; then
  echo "error: no Python export at $PY_SRC (pass path as \$1 or add python_export.jsonl)" >&2
  exit 2
fi

cp "$PY_SRC" "$PY_RAW"
normalize_ndjson "$PY_RAW" "$PY_NORM"

if ! diff -u "$REF_NORM" "$PY_NORM"; then
  echo >&2
  echo "error: wire NDJSON differs from Rust golden corpus (left=golden order, right=$PY_SRC)" >&2
  exit 1
fi

n="$(wc -l <"$REF_NORM" | tr -d '[:space:]')"
echo "ok: $PY_SRC matches golden wire corpus (${n} events, jq -S normalized)"
