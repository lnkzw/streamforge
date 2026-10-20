//! Contract tests for the v1 wire format, independent of the real engine.

use command_api::{
    Command, CommandError, CommandHandler, CommandList, CommandOutput, EngineStatus, ErrorCode,
    registry,
};
use control_protocol::{MAX_REQUEST_BYTES, handle_line, serve};
use serde_json::{Value, json};

/// Records calls and answers with a fixed status, or a fixed error if set.
#[derive(Default)]
struct FakeHandler {
    calls: Vec<Command>,
    fail_with: Option<CommandError>,
}

impl CommandHandler for FakeHandler {
    fn handle(&mut self, command: Command) -> Result<CommandOutput, CommandError> {
        self.calls.push(command.clone());
        if let Some(error) = &self.fail_with {
            return Err(error.clone());
        }
        Ok(match command {
            Command::GetStatus => CommandOutput::Status(EngineStatus {
                application: "Fake".to_owned(),
                version: "9.9.9".to_owned(),
                media_engine_ready: true,
                capture_available: false,
                streaming_active: false,
                streaming_state: None,
                recording_active: true,
                recording_failed: false,
                preview_active: false,
            }),
            Command::ListCommands => CommandOutput::Commands(CommandList::all()),
            other => {
                return Err(CommandError::new(
                    ErrorCode::Unsupported,
                    format!("fake handler does not implement `{}`", other.name()),
                ));
            }
        })
    }
}

fn respond(handler: &mut FakeHandler, line: &str) -> Value {
    let response = handle_line(line.as_bytes(), handler);
    let encoded = response.to_line();
    assert_eq!(encoded.last(), Some(&b'\n'));
    assert_eq!(
        encoded.iter().filter(|&&b| b == b'\n').count(),
        1,
        "response must be exactly one line"
    );
    serde_json::from_slice(&encoded).expect("response is JSON")
}

fn assert_error(response: &Value, id: &Value, code: &str) {
    assert_eq!(response["v"], 1, "{response}");
    assert_eq!(&response["id"], id, "{response}");
    assert_eq!(response["ok"], false, "{response}");
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], code, "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{response}"
    );
}

#[test]
fn get_status_success_envelope() {
    let mut handler = FakeHandler::default();
    let response = respond(&mut handler, r#"{"v":1,"id":"s-1","command":"get_status"}"#);
    assert_eq!(
        response,
        json!({
            "v": 1,
            "id": "s-1",
            "ok": true,
            "result": {
                "application": "Fake",
                "version": "9.9.9",
                "media_engine_ready": true,
                "capture_available": false,
                "streaming_active": false,
                "streaming_state": null,
                "recording_active": true,
                "recording_failed": false,
                "preview_active": false,
            },
        })
    );
    assert_eq!(handler.calls, vec![Command::GetStatus]);
}

#[test]
fn version_field_is_optional_and_integer_ids_are_echoed() {
    let mut handler = FakeHandler::default();
    let response = respond(
        &mut handler,
        r#"{"id":-42,"command":"get_status","params":null}"#,
    );
    assert_eq!(response["id"], -42);
    assert_eq!(response["ok"], true);
}

#[test]
fn list_commands_result_shape() {
    let mut handler = FakeHandler::default();
    let response = respond(
        &mut handler,
        r#"{"id":1,"command":"list_commands","params":{}}"#,
    );
    let commands = response["result"]["commands"]
        .as_array()
        .expect("commands array");
    assert_eq!(commands.len(), registry().len());
    for (command, descriptor) in commands.iter().zip(registry()) {
        assert_eq!(command["name"], descriptor.name);
        assert_eq!(command["summary"], descriptor.summary);
        assert!(
            ["read_only", "control", "sensitive"]
                .contains(&command["access"].as_str().unwrap_or("")),
            "{command}"
        );
    }
    assert_eq!(commands[0]["name"], "get_status");
    assert_eq!(commands[0]["access"], "read_only");
}

#[test]
fn malformed_requests_never_reach_the_handler() {
    let cases = [
        ("{not json", json!(null), "parse_error"),
        ("[1,2]", json!(null), "invalid_request"),
        ("\"get_status\"", json!(null), "invalid_request"),
        (
            r#"{"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (
            r#"{"id":null,"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (
            r#"{"id":1.5,"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (
            r#"{"id":{"x":1},"command":"get_status"}"#,
            json!(null),
            "invalid_request",
        ),
        (r#"{"id":"a"}"#, json!("a"), "invalid_request"),
        (r#"{"id":"b","command":7}"#, json!("b"), "invalid_request"),
        (
            r#"{"id":"c","command":"get_status","parms":{}}"#,
            json!("c"),
            "invalid_request",
        ),
        (
            r#"{"v":"1","id":"d","command":"get_status"}"#,
            json!("d"),
            "invalid_request",
        ),
        (
            r#"{"v":2,"id":"e","command":"get_status"}"#,
            json!("e"),
            "unsupported_version",
        ),
        (
            r#"{"id":"f","command":"start_teleport"}"#,
            json!("f"),
            "unknown_command",
        ),
        (
            r#"{"id":"g","command":"get_status","params":{"x":1}}"#,
            json!("g"),
            "invalid_params",
        ),
        (
            r#"{"id":"h","command":"get_status","params":"x"}"#,
            json!("h"),
            "invalid_params",
        ),
    ];
    let mut handler = FakeHandler::default();
    for (request, id, code) in cases {
        let response = respond(&mut handler, request);
        assert_error(&response, &id, code);
    }
    assert!(handler.calls.is_empty(), "{:?}", handler.calls);
}

#[test]
fn invalid_utf8_is_a_parse_error() {
    let mut handler = FakeHandler::default();
    let response = handle_line(b"{\"id\":\"\xff\"}", &mut handler);
    let response: Value = serde_json::from_slice(&response.to_line()).expect("JSON");
    assert_error(&response, &json!(null), "parse_error");
}

#[test]
fn error_details_are_structured() {
    let mut handler = FakeHandler::default();
    let unknown = respond(&mut handler, r#"{"id":1,"command":"nope"}"#);
    assert_eq!(unknown["error"]["details"]["command"], "nope");
    let available: Vec<_> = registry().iter().map(|d| d.name).collect();
    assert_eq!(unknown["error"]["details"]["available"], json!(available));

    let version = respond(&mut handler, r#"{"v":3,"id":2,"command":"get_status"}"#);
    assert_eq!(
        version["error"]["details"],
        json!({"requested": 3, "supported": [1]})
    );

    let field = respond(&mut handler, r#"{"id":3,"command":"get_status","x":0}"#);
    assert_eq!(field["error"]["details"], json!({"field": "x"}));
}

#[test]
fn handler_errors_are_mapped_with_details() {
    let mut handler = FakeHandler {
        fail_with: Some(
            CommandError::new(ErrorCode::InvalidState, "engine is busy")
                .with_details(json!({"state": "busy"})),
        ),
        ..FakeHandler::default()
    };
    let response = respond(&mut handler, r#"{"id":"busy","command":"get_status"}"#);
    assert_error(&response, &json!("busy"), "invalid_state");
    assert_eq!(response["error"]["message"], "engine is busy");
    assert_eq!(response["error"]["details"], json!({"state": "busy"}));
}

#[test]
fn serve_answers_each_request_in_order_and_stops_at_eof() {
    let mut oversized = vec![b'x'; MAX_REQUEST_BYTES + 1];
    oversized.push(b'\n');
    let mut input = Vec::new();
    input.extend_from_slice(b"{\"id\":1,\"command\":\"get_status\"}\n");
    input.extend_from_slice(b"\n  \r\n");
    input.extend_from_slice(b"garbage\n");
    input.extend_from_slice(&oversized);
    input.extend_from_slice(b"{\"id\":2,\"command\":\"list_commands\"}\r\n");
    input.extend_from_slice(b"{\"id\":3,\"command\":\"get_status\"}");

    let mut output = Vec::new();
    let mut handler = FakeHandler::default();
    serve(input.as_slice(), &mut output, &mut handler).expect("serve");

    let responses: Vec<Value> = output
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("JSON line"))
        .collect();
    assert_eq!(responses.len(), 5, "{responses:?}");
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["ok"], true);
    assert_error(&responses[1], &json!(null), "parse_error");
    assert_error(&responses[2], &json!(null), "request_too_large");
    assert_eq!(
        responses[2]["error"]["details"]["max_bytes"],
        MAX_REQUEST_BYTES
    );
    assert_eq!(responses[3]["id"], 2);
    assert_eq!(responses[3]["ok"], true);
    assert_eq!(responses[4]["id"], 3);
    assert_eq!(responses[4]["ok"], true);
    assert_eq!(
        handler.calls,
        vec![
            Command::GetStatus,
            Command::ListCommands,
            Command::GetStatus
        ]
    );
}

#[test]
fn probe_params_are_validated_before_the_handler() {
    let mut handler = FakeHandler::default();
    let response = respond(
        &mut handler,
        r#"{"id":"p","command":"probe_screen_capture","params":{"duration_ms":5}}"#,
    );
    assert_error(&response, &json!("p"), "invalid_params");
    assert_eq!(response["error"]["details"]["min_ms"], 100);
    assert!(handler.calls.is_empty());
}

#[test]
fn backend_error_codes_reach_the_wire() {
    let mut handler = FakeHandler::default();
    let response = respond(
        &mut handler,
        r#"{"id":"c","command":"get_capture_capabilities"}"#,
    );
    assert_error(&response, &json!("c"), "unsupported");
    assert_eq!(handler.calls, vec![Command::GetCaptureCapabilities]);
}
