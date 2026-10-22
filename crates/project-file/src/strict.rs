//! JSON into a [`Value`], refusing duplicate keys: `serde_json` keeps the
//! last of them, so `{"opacity": 5, "opacity": 0.5}` would load as 0.5
//! without a word.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// Parse `bytes`; a duplicate key is an error at its path.
pub(crate) fn parse(bytes: &[u8]) -> Result<Value, (String, String)> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value =
        serde_path_to_error::deserialize::<_, Strict>(&mut deserializer).map_err(|error| {
            // `.` is the whole file; `?` where the error left no path
            // (e.g. the file ends early).
            let path = error.path().to_string();
            let path = if matches!(path.as_str(), "." | "?") {
                String::new()
            } else {
                path
            };
            (path, error.into_inner().to_string())
        })?;
    // Characters after the value.
    deserializer
        .end()
        .map_err(|error| (String::new(), error.to_string()))?;
    Ok(value.0)
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("a number must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate field `{key}`")));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_refused_at_their_path() {
        let (path, message) = parse(br#"{"a": [{"b": 1, "b": 2}]}"#).unwrap_err();
        assert_eq!(path, "a[0]");
        assert!(
            message.starts_with("duplicate field `b` at line 1"),
            "{message}"
        );
        let value = parse(br#"{"a": [1, 2.5, "x", null, true, {"b": {}}]}"#).expect("valid");
        assert_eq!(
            value,
            serde_json::json!({"a": [1, 2.5, "x", null, true, {"b": {}}]})
        );
        let (path, message) = parse(b"{} x").unwrap_err();
        assert!(path.is_empty() && message.contains("trailing"), "{message}");
    }
}
