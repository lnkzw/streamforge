//! Schema versions 1 to 3's source fields, frozen for migration (ADR 0019).
//!
//! Version 4 adds `visible`, every source of an older file being visible,
//! and the kinds `image` and `camera`. The new field in an old file is
//! refused and removed, so the rest of the source is still checked; the new
//! kinds are refused.

use command_api::ProjectProblem;
use serde_json::{Map, Value};

use crate::problem;

/// Check a version 1 to 3 source object for version 4's fields and kinds;
/// returns the problems found.
pub(crate) fn migrate(object: &mut Map<String, Value>, at: &str) -> Vec<ProjectProblem> {
    let mut problems = Vec::new();
    if object.remove("visible").is_some() {
        problems.push(problem(
            &format!("{at}.visible"),
            "unknown field `visible` in schema version 1 to 3 (version 4 has it)".to_owned(),
        ));
    }
    if let Some(kind @ ("image" | "camera")) = object.get("kind").and_then(Value::as_str) {
        problems.push(problem(
            &format!("{at}.kind"),
            format!("unknown kind `{kind}` in schema version 1 to 3 (version 4 has it)"),
        ));
    }
    problems
}
