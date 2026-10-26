//! Schema versions 1 and 2's screen fields, frozen for migration (ADR 0018).
//!
//! Both versions stored `source_types` and `cursor`. Version 3 calls them
//! `target_kinds` and `cursor_mode`. Capture targets were never stored;
//! refuse the old `source_id` at its original path before command decoding.
//! New spellings in an old file are refused rather than overwriting data.

use command_api::ProjectProblem;
use serde_json::{Map, Value};

use crate::problem;

pub(crate) fn migrate(
    object: &mut Map<String, Value>,
    at: &str,
) -> Result<(), Vec<ProjectProblem>> {
    let mut problems = Vec::new();
    if object.contains_key("source_id") {
        problems.push(problem(
            &format!("{at}.source_id"),
            "what a screen source captures is not stored: it is machine-specific \
             (choose it after loading with set_capture_target)"
                .to_owned(),
        ));
    }
    if object.get("kind").and_then(Value::as_str) == Some("screen") {
        for (old, new) in [("source_types", "target_kinds"), ("cursor", "cursor_mode")] {
            if object.contains_key(new) {
                problems.push(problem(
                    &format!("{at}.{new}"),
                    format!("unknown field `{new}` in schema version 1 or 2; use `{old}`"),
                ));
            } else if let Some(value) = object.remove(old) {
                object.insert(new.to_owned(), value);
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
