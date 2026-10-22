//! Scene and source ids (ADR 0014): a type prefix and 12 random base-36
//! characters (about 62 bits), so ids from different projects do not
//! collide when scenes are copied between them.

use command_api::{CommandError, ErrorCode};

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const LENGTH: usize = 12;

/// A new id with `prefix` for which `taken` is false.
pub(crate) fn new_id(prefix: &str, taken: impl Fn(&str) -> bool) -> Result<String, CommandError> {
    loop {
        let id = random_id(prefix)?;
        if !taken(&id) {
            return Ok(id);
        }
    }
}

fn random_id(prefix: &str) -> Result<String, CommandError> {
    let mut value = getrandom::u64().map_err(|error| {
        CommandError::new(
            ErrorCode::Internal,
            format!("cannot generate an id: {error}"),
        )
    })?;
    let mut id = String::with_capacity(prefix.len() + LENGTH);
    id.push_str(prefix);
    for _ in 0..LENGTH {
        id.push(char::from(DIGITS[(value % 36) as usize]));
        value /= 36;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_prefix_and_twelve_base36_characters_and_skip_taken_ones() {
        let first = new_id("src_", |_| false).expect("id");
        let rest = first.strip_prefix("src_").expect("prefix");
        assert_eq!(rest.len(), LENGTH);
        assert!(rest.bytes().all(|byte| DIGITS.contains(&byte)), "{first}");
        // A taken id is drawn again.
        let calls = std::cell::Cell::new(0);
        let second = new_id("scene_", |_| {
            calls.set(calls.get() + 1);
            calls.get() == 1
        })
        .expect("id");
        assert_eq!(calls.get(), 2);
        assert!(second.starts_with("scene_"), "{second}");
    }
}
