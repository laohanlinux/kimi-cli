//! NDJSON golden fixtures under `tests/golden/` (wire `WireEvent` shapes).

const GOLDEN_FIXTURES: &[(&str, &str)] = &[
    ("minimal_turn.jsonl", include_str!("golden/minimal_turn.jsonl")),
    ("more_events.jsonl", include_str!("golden/more_events.jsonl")),
    ("session_shutdown.jsonl", include_str!("golden/session_shutdown.jsonl")),
];

fn assert_jsonl_wire_roundtrip(name: &str, raw: &str) -> usize {
    let mut n = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: rki_rs::wire::WireEvent = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{name}: parse line {line:?}: {e}"));
        let again = serde_json::to_string(&ev).expect("serialize");
        let ev2: rki_rs::wire::WireEvent = serde_json::from_str(&again).expect("roundtrip");
        assert_eq!(format!("{ev:?}"), format!("{ev2:?}"), "{name}: Debug mismatch after roundtrip");
        n += 1;
    }
    n
}

#[test]
fn python_export_sample_matches_fixture_concat() {
    let mut expected = String::new();
    expected.push_str(include_str!("golden/minimal_turn.jsonl"));
    expected.push_str(include_str!("golden/more_events.jsonl"));
    expected.push_str(include_str!("golden/session_shutdown.jsonl"));
    let sample = include_str!("golden/python_export.sample.jsonl");
    assert_eq!(
        sample, expected.as_str(),
        "python_export.sample.jsonl must equal minimal_turn + more_events + session_shutdown (canonical order for diff_golden_vs_python_export.sh)"
    );
}

#[test]
fn golden_all_jsonl_roundtrip() {
    let mut total = 0usize;
    for (name, raw) in GOLDEN_FIXTURES {
        let n = assert_jsonl_wire_roundtrip(name, raw);
        assert!(n > 0, "{name}: expected at least one wire event");
        total += n;
    }
    assert_eq!(total, 9, "expected combined event count across fixtures");
}
