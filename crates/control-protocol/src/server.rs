//! Request decoding and the blocking line-oriented serve loop.

use std::io::{self, BufRead, Read, Write};

use command_api::{Command, CommandHandler};
use serde_json::{Value, json};

use crate::{ErrorCode, MAX_REQUEST_BYTES, PROTOCOL_VERSION, RequestId, Response, ResponseError};

/// Top-level request fields besides `id`. Unknown fields are rejected so typos
/// such as `parms` fail loudly instead of being ignored.
const KNOWN_FIELDS: [&str; 3] = ["v", "command", "params"];

struct Request {
    id: RequestId,
    command: String,
    params: Option<Value>,
}

type RequestFailure = (Option<RequestId>, ResponseError);

/// Decode one request line (without its terminator), execute it, and build the
/// response. Never panics on malformed input.
pub fn handle_line<H: CommandHandler + ?Sized>(line: &[u8], handler: &mut H) -> Response {
    let request = match parse_request(line) {
        Ok(request) => request,
        Err((id, error)) => return Response::failure(id, error),
    };
    let outcome = Command::decode(&request.command, request.params.as_ref())
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

    let command = match object.remove("command") {
        Some(Value::String(command)) => command,
        Some(_) => return Err((Some(id), invalid_request("`command` must be a string"))),
        None => return Err((Some(id), invalid_request("missing `command`"))),
    };

    Ok(Request {
        id,
        command,
        params: object.remove("params"),
    })
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
