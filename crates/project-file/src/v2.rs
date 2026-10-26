//! Schema versions 1 and 2's screen fields, frozen for migration (ADR 0018).
//!
//! Both versions stored `source_types` and `cursor`. Version 3 calls them
//! `target_kinds` and `cursor_mode`. Capture targets were never stored: an
//! old `source_id` is refused at its own path and removed, so the rest of the
//! source is still checked. New spellings in an old file are refused and
//! removed rather than overwriting the old ones.

use command_api::ProjectProblem;
use serde_json::{Map, Value};

use crate::problem;

/// Rename a version 1 or 2 source object's screen fields in place; returns
/// the problems found on the way.
pub(crate) fn migrate(object: &mut Map<String, Value>, at: &str) -> Vec<ProjectProblem> {
    let mut problems = Vec::new();
    if object.remove("source_id").is_some() {
        problems.push(problem(
            &format!("{at}.source_id"),
            "what a screen source captures is not stored: it is machine-specific \
             (choose it after loading with set_capture_target)"
                .to_owned(),
        ));
    }
    if object.get("kind").and_then(Value::as_str) == Some("screen") {
        for (old, new) in [("source_types", "target_kinds"), ("cursor", "cursor_mode")] {
            if object.remove(new).is_some() {
                problems.push(problem(
                    &format!("{at}.{new}"),
                    format!("unknown field `{new}` in schema version 1 or 2; use `{old}`"),
                ));
            }
            if let Some(value) = object.remove(old) {
                object.insert(new.to_owned(), value);
            }
        }
    }
    problems
}
