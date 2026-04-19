#!/usr/bin/env bash
# Validate NDJSON golden fixtures (WireEvent per line) and optional Python export diff.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -q --test golden_wire
bash scripts/diff_golden_vs_python_export.sh
