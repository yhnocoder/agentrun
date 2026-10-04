pub mod fake;

use std::path::{Path, PathBuf};
use std::time::Instant;

use agentrun::adapter::Adapter;
use agentrun::aggregate::Aggregator;
use agentrun::run::{Exit, conclude, translate_line};
use serde_json::Value;

pub fn replay(adapter: &mut dyn Adapter, raw: &Path, prompt: &str) -> Vec<Value> {
    let started = Instant::now();
    let content = std::fs::read(raw).expect("raw fixture is readable");
    let mut aggregator = Aggregator::new(adapter.echoes_prompt());
    let mut events = aggregator.begin(prompt);
    for line in content.split(|byte| *byte == b'\n') {
        events.extend(translate_line(line, adapter, &mut aggregator).0);
    }
    let exit = Exit {
        code: Some(0),
        signal: None,
        timed_out: false,
    };
    let (rest, _) = conclude(aggregator, adapter, &exit, "", started);
    events.extend(rest);
    events
        .iter()
        .map(|event| without_timing(serde_json::from_str(&event.to_json()).expect("event is JSON")))
        .collect()
}

pub fn without_timing(mut event: Value) -> Value {
    if let Some(fields) = event.as_object_mut() {
        fields.remove("time");
        fields.remove("duration_ms");
    }
    event
}

pub fn fixture(runtime: &str, name: &str, suffix: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(runtime)
        .join(format!("{name}.{suffix}"))
}

pub fn assert_replay(adapter: &mut dyn Adapter, runtime: &str, name: &str, prompt: &str) {
    let actual = replay(adapter, &fixture(runtime, name, "raw.jsonl"), prompt);
    let expected: Vec<Value> = std::fs::read_to_string(fixture(runtime, name, "events.jsonl"))
        .expect("events fixture is readable")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| without_timing(serde_json::from_str(line).expect("expected event is JSON")))
        .collect();
    assert_eq!(actual, expected);
}
