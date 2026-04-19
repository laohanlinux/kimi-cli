# Golden wire fixtures (NDJSON)

Each line is one JSON value deserializable as `rki_rs::wire::WireEvent` (same `type` tagging as runtime).

Use these to:

- Lock stable wire shapes for parity checks; `bash scripts/diff_golden_vs_python_export.sh` diffs a Python-exported NDJSON file (or the checked-in `python_export.sample.jsonl`) against the canonical golden concat.
- Regression-test serde when extending `WireEvent`.

Convention:

- One logical turn per file or multi-turn sequences in order.
- No `WireEnvelope` wrapper (timestamps / `event_id` are non-deterministic).

CI / local: run `bash scripts/check_golden.sh` from the `rki-rs` crate root (`golden_wire` + `diff_golden_vs_python_export.sh`). Requires `jq` on PATH for the diff step.

Drop an export from kimi-cli as `tests/golden/python_export.jsonl` (same line schema as fixtures); if absent, the diff script compares against `python_export.sample.jsonl` so CI stays green until a real export is added.

Fixtures: `minimal_turn.jsonl` (turn lifecycle), `more_events.jsonl` (additional `WireEvent` variants), `session_shutdown.jsonl` (L35 shutdown signal).

CLI multimodal: same JSON schemas as `rki_rs::turn_input::parse_cli_turn_line` (one line to stdin or `--print`).
