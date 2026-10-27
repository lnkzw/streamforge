//! Schema versions 1 to 3's source fields, frozen for migration (ADR 0019).
//!
//! Version 4 adds `visible`; every source of an older file is visible. The
//! new field in an old file is refused and removed, so the rest of the
//! source is still checked.

use command_api::ProjectProblem;
use serde_json::{Map, Value};

use crate::problem;

/// Check a version 1 to 3 source object for version 4's fields; returns
/// the problems found.
pub(crate) fn migrate(object: &mut Map<String, Value>, at: &str) -> Vec<ProjectProblem> {
    let mut problems = Vec::new();
    if object.remove("visible").is_some() {
        problems.push(problem(
            &format!("{at}.visible"),
            "unknown field `visible` in schema version 1 to 3 (version 4 has it)".to_owned(),
        ));
    }
    problems
}
