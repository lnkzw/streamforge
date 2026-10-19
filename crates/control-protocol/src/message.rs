//! Wire representation of responses, request IDs, and error codes.

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::PROTOCOL_VERSION;

/// Client-chosen request identifier: a JSON string or an integer in `i64` range.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    Integer(i64),
    String(String),
}

impl RequestId {
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(id) => Some(Self::String(id.clone())),
            Value::Number(id) => id.as_i64().map(Self::Integer),
            _ => None,
        }
    }
}

/// Stable wire error codes.
///
/// Mapped explicitly from [`command_api::ErrorCode`] so that internal renames
/// cannot change the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line is not valid UTF-8 JSON.
    ParseError,
    /// Valid JSON, but not a well-formed request envelope.
    InvalidRequest,
    /// The line exceeds [`crate::MAX_REQUEST_BYTES`].
    RequestTooLarge,
    /// The request asks for a protocol version this engine does not speak.
    UnsupportedVersion,
    UnknownCommand,
    InvalidParams,
    InvalidState,
    Internal,
}

impl From<command_api::ErrorCode> for ErrorCode {
    fn from(code: command_api::ErrorCode) -> Self {
        match code {
            command_api::ErrorCode::UnknownCommand => Self::UnknownCommand,
            command_api::ErrorCode::InvalidParams => Self::InvalidParams,
            command_api::ErrorCode::InvalidState => Self::InvalidState,
            command_api::ErrorCode::Internal => Self::Internal,
        }
    }
}

/// The `error` object of a failed response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ResponseError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<command_api::CommandError> for ResponseError {
    fn from(error: command_api::CommandError) -> Self {
        Self {
            code: error.code.into(),
            message: error.message,
            details: error.details,
        }
    }
}

/// One response line.
///
/// `id` is `None` only when the request ID could not be determined (parse
/// failure, missing or malformed `id`, oversized line); it is sent as `null`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub id: Option<RequestId>,
    pub outcome: Result<Value, ResponseError>,
}

/// Last-resort response if encoding fails twice; keeps the stream well-formed.
const ENCODE_FAILURE_JSON: &[u8] =
    b"{\"v\":1,\"id\":null,\"ok\":false,\"error\":{\"code\":\"internal\",\"message\":\"failed to encode response\"}}";

impl Response {
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            id: Some(id),
            outcome: Ok(result),
        }
    }

    pub fn failure(id: Option<RequestId>, error: ResponseError) -> Self {
        Self {
            id,
            outcome: Err(error),
        }
    }

    /// Encode as a single JSON line including the terminating `\n`.
    pub fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_else(|err| {
            let fallback = Self::failure(
                self.id.clone(),
                ResponseError::new(
                    ErrorCode::Internal,
                    format!("failed to encode response: {err}"),
                ),
            );
            serde_json::to_vec(&fallback).unwrap_or_else(|_| ENCODE_FAILURE_JSON.to_vec())
        });
        line.push(b'\n');
        line
    }
}

#[derive(Serialize)]
struct WireResponse<'a> {
    v: u64,
    id: Option<&'a RequestId>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ResponseError>,
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireResponse {
            v: PROTOCOL_VERSION,
            id: self.id.as_ref(),
            ok: self.outcome.is_ok(),
            result: self.outcome.as_ref().ok(),
            error: self.outcome.as_ref().err(),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn success_envelope_is_one_line() {
        let response = Response::success(RequestId::Integer(1), json!({}));
        assert_eq!(
            response.to_line(),
            b"{\"v\":1,\"id\":1,\"ok\":true,\"result\":{}}\n"
        );
    }

    #[test]
    fn failure_envelope_has_null_id_when_unknown() {
        let response = Response::failure(
            None,
            ResponseError::new(ErrorCode::InvalidRequest, "bad request"),
        );
        assert_eq!(
            String::from_utf8(response.to_line()).expect("utf-8"),
            "{\"v\":1,\"id\":null,\"ok\":false,\"error\":{\"code\":\"invalid_request\",\"message\":\"bad request\"}}\n"
        );
    }

    #[test]
    fn fallback_line_is_valid_failure_envelope() {
        let value: Value = serde_json::from_slice(ENCODE_FAILURE_JSON).expect("valid JSON");
        assert_eq!(value["v"], PROTOCOL_VERSION);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "internal");
    }

    #[test]
    fn error_codes_have_stable_wire_names() {
        let codes = [
            (ErrorCode::ParseError, "parse_error"),
            (ErrorCode::InvalidRequest, "invalid_request"),
            (ErrorCode::RequestTooLarge, "request_too_large"),
            (ErrorCode::UnsupportedVersion, "unsupported_version"),
            (ErrorCode::UnknownCommand, "unknown_command"),
            (ErrorCode::InvalidParams, "invalid_params"),
            (ErrorCode::InvalidState, "invalid_state"),
            (ErrorCode::Internal, "internal"),
        ];
        for (code, name) in codes {
            assert_eq!(serde_json::to_value(code).expect("serialize"), name);
        }
    }

    #[test]
    fn command_error_codes_keep_their_names_on_the_wire() {
        for code in [
            command_api::ErrorCode::UnknownCommand,
            command_api::ErrorCode::InvalidParams,
            command_api::ErrorCode::InvalidState,
            command_api::ErrorCode::Internal,
        ] {
            let wire = serde_json::to_value(ErrorCode::from(code)).expect("serialize");
            assert_eq!(wire, code.as_str());
        }
    }

    #[test]
    fn request_ids_accept_strings_and_integers_only() {
        assert_eq!(
            RequestId::from_json(&json!("a")),
            Some(RequestId::String("a".into()))
        );
        assert_eq!(
            RequestId::from_json(&json!(-7)),
            Some(RequestId::Integer(-7))
        );
        for invalid in [
            json!(1.5),
            json!(u64::MAX),
            json!(null),
            json!(true),
            json!([1]),
            json!({}),
        ] {
            assert_eq!(RequestId::from_json(&invalid), None, "{invalid}");
        }
    }
}
