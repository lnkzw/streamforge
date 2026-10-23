//! Finding what a command names by id, by name or by both (ADR 0014): the
//! id is looked up first, and a name given as well must match it.

use command_api::{CommandError, ErrorCode};
use serde_json::json;

/// What is looked up, for messages: e.g. `scene` with its id parameter
/// `scene_id`.
pub(crate) struct Kind {
    pub what: &'static str,
    pub id_field: &'static str,
}

/// The position in `items` of the one `id` and `name` name; `key` gives an
/// item's id and name.
pub(crate) fn position<T>(
    items: &[T],
    id: Option<&str>,
    name: Option<&str>,
    kind: &Kind,
    key: impl Fn(&T) -> (&str, &str),
) -> Result<usize, CommandError> {
    let what = kind.what;
    match (id, name) {
        (Some(id), name) => {
            let index = items
                .iter()
                .position(|item| key(item).0 == id)
                .ok_or_else(|| not_found_id(what, kind.id_field, id))?;
            let found = key(&items[index]).1;
            match name {
                Some(name) if name != found => Err(mismatch(format!(
                    "{what} `{id}` is named `{found}`, not `{name}`"
                ))),
                _ => Ok(index),
            }
        }
        (None, Some(name)) => items
            .iter()
            .position(|item| key(item).1 == name)
            .ok_or_else(|| not_found(what, name)),
        (None, None) => Err(mismatch(format!(
            "no {what} given: pass its `{}` or its name",
            kind.id_field
        ))),
    }
}

pub(crate) fn not_found(what: &str, name: &str) -> CommandError {
    CommandError::new(ErrorCode::NotFound, format!("no {what} named `{name}`"))
        .with_details(json!({ what: name }))
}

pub(crate) fn not_found_id(what: &str, field: &str, id: &str) -> CommandError {
    CommandError::new(ErrorCode::NotFound, format!("no {what} with id `{id}`"))
        .with_details(json!({ field: id }))
}

/// An id and a name (or a scene) that do not name the same thing.
pub(crate) fn mismatch(message: impl Into<String>) -> CommandError {
    CommandError::new(ErrorCode::InvalidParams, message)
}
