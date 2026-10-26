//! Request decoding and the blocking line-oriented serve loop.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::io::{self, BufRead, Read, Write};

use command_api::{Command, CommandError, CommandHandler};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};

use crate::{ErrorCode, MAX_REQUEST_BYTES, PROTOCOL_VERSION, RequestId, Response, ResponseError};

/// Top-level request fields besides `id`. Unknown fields are rejected so typos
/// such as `parms` fail loudly instead of being ignored.
const KNOWN_FIELDS: [&str; 3] = ["v", "command", "params"];

struct Request {
    id: RequestId,
    command: String,
    params: Option<Value>,
    /// The first key that occurs twice inside `params`, as a path in it.
    duplicate_param: Option<String>,
}

type RequestFailure = (Option<RequestId>, ResponseError);

/// Decode one request line (without its terminator), execute it, and build the
/// response. Never panics on malformed input.
pub fn handle_line<H: CommandHandler + ?Sized>(line: &[u8], handler: &mut H) -> Response {
    let request = match parse_request(line) {
        Ok(request) => request,
        Err((id, error)) => return Response::failure(id, error),
    };
    let decoded = Command::decode(&request.command, request.params.as_ref());
    let decoded = match (decoded, request.duplicate_param) {
        // An unknown command says more than its parameters.
        (Err(error), _) if error.code == command_api::ErrorCode::UnknownCommand => Err(error),
        (_, Some(path)) => Err(CommandError::new(
            command_api::ErrorCode::InvalidParams,
            format!("duplicate key `{path}` in `params`"),
        )
        .with_details(json!({ "command": request.command, "duplicate": path }))),
        (decoded, None) => decoded,
    };
    let outcome = decoded
        .and_then(|command| handler.handle(command))
        .map_err(ResponseError::from)
        .and_then(|output| {
            serde_json::to_value(output).map_err(|err| {
                ResponseError::new(
                    ErrorCode::Internal,
                    format!("failed to encode command result: {err}"),
                )
            })
        });
    Response {
        id: Some(request.id),
        outcome,
    }
}

/// Serve requests from `reader` until EOF, writing one response per request.
///
/// Requests are handled sequentially and answered in order; clients should
/// still correlate by `id`. Blank lines are ignored. Returns an error only for
/// I/O failures on the underlying streams.
pub fn serve<R, W, H>(mut reader: R, mut writer: W, handler: &mut H) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    H: CommandHandler + ?Sized,
{
    let mut line = Vec::new();
    loop {
        let response = match read_line_bounded(&mut reader, &mut line, MAX_REQUEST_BYTES)? {
            Line::Eof => return Ok(()),
            Line::TooLong => Response::failure(
                None,
                ResponseError::new(
                    ErrorCode::RequestTooLarge,
                    format!("request line exceeds {MAX_REQUEST_BYTES} bytes"),
                )
                .with_details(json!({ "max_bytes": MAX_REQUEST_BYTES })),
            ),
            Line::Complete if line.iter().all(u8::is_ascii_whitespace) => continue,
            Line::Complete => handle_line(&line, handler),
        };
        writer.write_all(&response.to_line())?;
        writer.flush()?;
    }
}

fn parse_request(line: &[u8]) -> Result<Request, RequestFailure> {
    let text = std::str::from_utf8(line).map_err(|err| {
        (
            None,
            ResponseError::new(
                ErrorCode::ParseError,
                format!("request is not valid UTF-8: {err}"),
            ),
        )
    })?;
    let value: Value = serde_json::from_str(text).map_err(|err| {
        (
            None,
            ResponseError::new(ErrorCode::ParseError, format!("invalid JSON: {err}")),
        )
    })?;
    let Value::Object(mut object) = value else {
        return Err((None, invalid_request("request must be a JSON object")));
    };
    // `serde_json` keeps the last of duplicate keys; find them, so a
    // conflicting request fails instead of running with a guess.
    let duplicate = duplicate_key(text);
    // A key repeated at the top level (`params` too), or inside a field
    // other than `params`.
    let duplicate_top = duplicate
        .as_ref()
        .filter(|path| path.len() == 1 || path[0] != "params");
    if duplicate_top.is_some_and(|path| path.first().is_some_and(|first| first == "id")) {
        // Which `id` to echo is unknown.
        return Err((None, invalid_request("duplicate key `id`")));
    }

    // Resolve the ID first so every later error can still be correlated.
    let id = match object.remove("id") {
        Some(raw) => RequestId::from_json(&raw).ok_or_else(|| {
            (
                None,
                invalid_request("`id` must be a string or an integer in i64 range"),
            )
        })?,
        None => return Err((None, invalid_request("missing `id`"))),
    };

    if let Some(path) = duplicate_top {
        let path = display_path(path);
        return Err((
            Some(id),
            invalid_request(format!("duplicate key `{path}`"))
                .with_details(json!({ "duplicate": path })),
        ));
    }

    // The version before the fields: a later version may have other fields,
    // and its client needs to hear which versions this engine speaks.
    match object.remove("v") {
        None => {}
        Some(Value::Number(v)) => match v.as_u64() {
            Some(PROTOCOL_VERSION) => {}
            Some(requested) => {
                return Err((
                    Some(id),
                    ResponseError::new(
                        ErrorCode::UnsupportedVersion,
                        format!("protocol version {requested} is not supported"),
                    )
                    .with_details(json!({
                        "requested": requested,
                        "supported": [PROTOCOL_VERSION],
                    })),
                ));
            }
            None => return Err((Some(id), invalid_version())),
        },
        Some(_) => return Err((Some(id), invalid_version())),
    }

    let unknown = object
        .keys()
        .find(|key| !KNOWN_FIELDS.contains(&key.as_str()))
        .cloned();
    if let Some(field) = unknown {
        return Err((
            Some(id),
            invalid_request(format!("unknown field `{field}`"))
                .with_details(json!({ "field": field })),
        ));
    }

    let command = match object.remove("command") {
        Some(Value::String(command)) => command,
        Some(_) => return Err((Some(id), invalid_request("`command` must be a string"))),
        None => return Err((Some(id), invalid_request("missing `command`"))),
    };

    Ok(Request {
        id,
        command,
        params: object.remove("params"),
        duplicate_param: duplicate
            .filter(|path| path.len() > 1)
            .map(|path| display_path(&path[1..])),
    })
}

/// The path of the first duplicate key in `text` (valid JSON), e.g.
/// `["params", "tones", "0", "gain_db"]`; array indices are numbers.
fn duplicate_key(text: &str) -> Option<Vec<String>> {
    let found = RefCell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let check = DuplicateCheck {
        path: Vec::new(),
        found: &found,
    };
    // The text already parsed as JSON, so an error here is not expected;
    // without a path, nothing is reported.
    check.deserialize(&mut deserializer).ok()?;
    found.into_inner()
}

/// `tones[0].gain_db` for `["tones", "0", "gain_db"]`.
fn display_path(path: &[String]) -> String {
    let mut shown = String::new();
    for segment in path {
        if segment.parse::<usize>().is_ok() {
            shown.push('[');
            shown.push_str(segment);
            shown.push(']');
        } else {
            if !shown.is_empty() {
                shown.push('.');
            }
            shown.push_str(segment);
        }
    }
    shown
}

/// Walks a JSON value and records the first key that repeats in an object.
struct DuplicateCheck<'a> {
    path: Vec<String>,
    found: &'a RefCell<Option<Vec<String>>>,
}

impl DuplicateCheck<'_> {
    fn child(&self, segment: String) -> Self {
        let mut path = self.path.clone();
        path.push(segment);
        DuplicateCheck {
            path,
            found: self.found,
        }
    }
}

impl<'de> DeserializeSeed<'de> for DuplicateCheck<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DuplicateCheck<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        while seq
            .next_element_seed(self.child(index.to_string()))?
            .is_some()
        {
            index += 1;
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.found.borrow().is_some() {
                // Reported already; read past the rest.
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let child = self.child(key.clone());
            if !keys.insert(key) {
                *self.found.borrow_mut() = Some(child.path.clone());
            }
            map.next_value_seed(child)?;
        }
        Ok(())
    }
}

fn invalid_request(message: impl Into<String>) -> ResponseError {
    ResponseError::new(ErrorCode::InvalidRequest, message)
}

fn invalid_version() -> ResponseError {
    invalid_request("`v` must be a non-negative integer protocol version")
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// `line` holds one request without its terminator (`\n` or `\r\n`).
    Complete,
    /// The line exceeded the limit and was discarded up to the next `\n`.
    TooLong,
    Eof,
}

/// Read one line into `line`, buffering at most `max_bytes + 1` bytes.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Line> {
    line.clear();
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let read = (&mut *reader).take(limit).read_until(b'\n', line)?;
    if read == 0 {
        return Ok(Line::Eof);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        return Ok(Line::Complete);
    }
    if line.len() > max_bytes {
        line.clear();
        reader.skip_until(b'\n')?;
        return Ok(Line::TooLong);
    }
    // Final line without a terminator.
    Ok(Line::Complete)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn read_all(input: &[u8], max_bytes: usize) -> Vec<(Line, Vec<u8>)> {
        let mut reader = Cursor::new(input);
        let mut line = Vec::new();
        let mut lines = Vec::new();
        loop {
            let kind = read_line_bounded(&mut reader, &mut line, max_bytes).expect("read");
            if kind == Line::Eof {
                return lines;
            }
            lines.push((kind, line.clone()));
        }
    }

    #[test]
    fn line_at_limit_is_complete() {
        assert_eq!(
            read_all(b"abcd\n", 4),
            vec![(Line::Complete, b"abcd".to_vec())]
        );
    }

    #[test]
    fn line_over_limit_is_discarded_and_next_line_survives() {
        assert_eq!(
            read_all(b"abcde\nok\n", 4),
            vec![
                (Line::TooLong, Vec::new()),
                (Line::Complete, b"ok".to_vec())
            ]
        );
    }

    #[test]
    fn very_long_line_is_skipped_without_buffering() {
        let mut input = vec![b'x'; 10_000];
        input.extend_from_slice(b"\nok");
        assert_eq!(
            read_all(&input, 16),
            vec![
                (Line::TooLong, Vec::new()),
                (Line::Complete, b"ok".to_vec())
            ]
        );
    }

    #[test]
    fn crlf_and_missing_final_terminator_are_accepted() {
        assert_eq!(
            read_all(b"a\r\nb", 4),
            vec![
                (Line::Complete, b"a".to_vec()),
                (Line::Complete, b"b".to_vec())
            ]
        );
    }

    #[test]
    fn empty_input_is_eof() {
        assert!(read_all(b"", 4).is_empty());
    }
}
