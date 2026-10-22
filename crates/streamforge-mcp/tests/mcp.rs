//! M8 acceptance: the official MCP client (rmcp) drives the real
//! `streamforge-mcp` binary over stdio.

use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::process::Command;

type Client = RunningService<RoleClient, ()>;

/// Tests that run encoders take turns, so small CI runners keep up.
static MEDIA: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("streamforge-mcp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn connect(dir: &TempDir, args: &[&str], env: &[(&str, &str)]) -> Client {
    let command = Command::new(env!("CARGO_BIN_EXE_streamforge-mcp")).configure(|command| {
        command.arg("--output-dir").arg(&dir.0).args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        command.stderr(Stdio::null());
    });
    let transport = TokioChildProcess::new(command).expect("spawn server");
    tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .expect("initialize in time")
        .expect("initialize")
}

async fn tools(client: &Client) -> Vec<Tool> {
    client.list_all_tools().await.expect("list tools")
}

fn names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_ref()).collect()
}

async fn call(client: &Client, name: &str, arguments: Value) -> CallToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("arguments must be an object");
    };
    let request = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
    tokio::time::timeout(Duration::from_secs(20), client.call_tool(request))
        .await
        .expect("answer in time")
        .expect("tool call")
}

fn ok(result: &CallToolResult) -> &Value {
    assert_eq!(result.is_error, Some(false), "{result:?}");
    result
        .structured_content
        .as_ref()
        .expect("structured result")
}

fn error_code(result: &CallToolResult) -> &str {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    result
        .structured_content
        .as_ref()
        .expect("structured error")["code"]
        .as_str()
        .expect("code")
}

#[tokio::test]
async fn read_only_by_default() {
    let dir = TempDir::new("read-only");
    let client = connect(&dir, &[], &[]).await;
    let info = client.peer_info().expect("server info");
    assert!(
        info.instructions
            .as_deref()
            .is_some_and(|text| text.contains("read-only"))
    );
    let tools = tools(&client).await;
    let listed = names(&tools);
    for name in [
        "get_status",
        "get_health",
        "list_commands",
        "list_scenes",
        "get_mixer",
    ] {
        assert!(listed.contains(&name), "{name} missing from {listed:?}");
    }
    for name in [
        "create_scene",
        "start_recording",
        "start_streaming",
        "set_preview",
    ] {
        assert!(!listed.contains(&name), "{name} offered read-only");
    }
    assert!(tools.iter().all(|tool| {
        tool.annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            == Some(true)
    }));
    let status = call(&client, "get_status", json!({})).await;
    assert_eq!(ok(&status)["application"], "StreamForge");
    // Not offered and not callable either; the denial names what this call
    // needs (control for color bars, not the sensitive needed for screens).
    let create = call(&client, "create_scene", json!({"name": "Sneaky"})).await;
    assert_eq!(error_code(&create), "permission_denied");
    let bars = call(
        &client,
        "add_source",
        json!({"scene": "Test pattern", "name": "b", "kind": "color_bars"}),
    )
    .await;
    assert_eq!(error_code(&bars), "permission_denied");
    let details = &bars.structured_content.as_ref().expect("error")["details"];
    assert_eq!(
        (&details["required"], &details["allowed"]),
        (&json!("control"), &json!("read_only"))
    );
    // Unknown tools are protocol errors.
    let unknown = client
        .call_tool(CallToolRequestParams::new("format_disk"))
        .await;
    assert!(unknown.is_err());
    client.cancel().await.expect("close");
}

#[tokio::test]
async fn control_lets_an_agent_create_a_test_scene() {
    let dir = TempDir::new("control");
    let client = connect(&dir, &["--allow", "control"], &[]).await;
    let tools = tools(&client).await;
    let listed = names(&tools);
    assert!(listed.contains(&"create_scene") && listed.contains(&"add_source"));
    assert!(!listed.contains(&"delete_scene") && !listed.contains(&"start_recording"));
    let add_tool = tools
        .iter()
        .find(|tool| tool.name == "add_source")
        .expect("add_source");
    let schema: &JsonObject = &add_tool.input_schema;
    assert_eq!(schema["required"], json!(["scene", "name", "kind"]));

    ok(&call(&client, "create_scene", json!({"name": "Agent test"})).await);
    ok(&call(
        &client,
        "add_source",
        json!({"scene": "Agent test", "name": "bars", "kind": "color_bars"}),
    )
    .await);
    let scenes = call(&client, "list_scenes", json!({})).await;
    let scene = ok(&scenes)["scenes"]
        .as_array()
        .expect("scenes")
        .iter()
        .find(|scene| scene["name"] == "Agent test")
        .cloned()
        .expect("created scene");
    assert_eq!(scene["sources"][0]["kind"], "color_bars");
    // Screen capture and deleting need sensitive access.
    let screen = call(
        &client,
        "add_source",
        json!({"scene": "Agent test", "name": "desk", "kind": "screen"}),
    )
    .await;
    assert_eq!(error_code(&screen), "permission_denied");
    let delete = call(
        &client,
        "delete_scene",
        json!({"name": "Agent test", "confirm": true}),
    )
    .await;
    assert_eq!(error_code(&delete), "permission_denied");
    // Engine validation errors come back structured.
    let invalid = call(&client, "create_scene", json!({"name": ""})).await;
    assert_eq!(error_code(&invalid), "invalid_params");
    client.cancel().await.expect("close");
}

#[tokio::test]
async fn streaming_needs_a_configured_target() {
    let dir = TempDir::new("no-target");
    let client = connect(&dir, &["--allow", "sensitive"], &[]).await;
    let listed = names(&tools(&client).await).join(",");
    assert!(listed.contains("start_recording") && listed.contains("delete_scene"));
    assert!(!listed.contains("start_streaming"));
    let stream = call(
        &client,
        "start_streaming",
        json!({"url": "rtmp://attacker.example/app"}),
    )
    .await;
    assert_eq!(error_code(&stream), "permission_denied");
    client.cancel().await.expect("close");
}

#[test]
fn a_streaming_target_needs_sensitive_access_and_a_set_key() {
    let dir = TempDir::new("startup");
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_streamforge-mcp"))
            .arg("--output-dir")
            .arg(&dir.0)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (started, message) = run(&["--stream-url", "rtmp://host/app"], &[]);
    assert!(
        !started && message.contains("--allow sensitive"),
        "{message}"
    );
    // The variable name is not echoed: it might be the key itself.
    let (started, message) = run(
        &[
            "--allow",
            "sensitive",
            "--stream-url",
            "rtmp://host/app",
            "--stream-key-env",
            "live_SECRET_passed_by_mistake",
        ],
        &[],
    );
    assert!(!started && message.contains("not set"), "{message}");
    assert!(!message.contains("SECRET"), "{message}");
    let (started, message) = run(
        &[
            "--allow",
            "sensitive",
            "--stream-url",
            "rtmp://host/app",
            "--stream-key-env",
            "STREAMFORGE_TEST_EMPTY_KEY",
        ],
        &[("STREAMFORGE_TEST_EMPTY_KEY", "")],
    );
    assert!(!started && message.contains("empty"), "{message}");
    let (started, message) = run(
        &[
            "--allow",
            "sensitive",
            "--stream-url",
            "rtmp://host/app",
            "--stream-key-env",
            "STREAMFORGE_TEST_BAD_KEY",
        ],
        &[("STREAMFORGE_TEST_BAD_KEY", "bad/SECRET")],
    );
    assert!(!started && !message.contains("SECRET"), "{message}");
}

#[tokio::test]
async fn agents_stream_only_to_the_configured_target_and_never_see_the_key() {
    let _media = MEDIA.lock().await;
    use rtmp_test_server::{ServerOptions, TestServer};
    use std::time::Instant;

    let dir = TempDir::new("stream");
    // The ingest waits for one publisher; starting the MCP server first can
    // take longer than the default accept timeout on a loaded runner.
    let ingest = TestServer::start(ServerOptions {
        accept_timeout: Duration::from_secs(60),
        ..ServerOptions::default()
    });
    let url = ingest.url();
    let client = connect(
        &dir,
        &[
            "--allow",
            "sensitive",
            "--stream-url",
            &url,
            "--stream-key-env",
            "SF_MCP_KEY",
        ],
        &[("SF_MCP_KEY", "mcp-SECRET-key")],
    )
    .await;
    let tools = tools(&client).await;
    let stream_tool = tools
        .iter()
        .find(|tool| tool.name == "start_streaming")
        .expect("start_streaming offered");
    let properties = stream_tool.input_schema["properties"]
        .as_object()
        .expect("properties");
    assert!(!properties.contains_key("url") && !properties.contains_key("stream_key"));
    assert_eq!(
        stream_tool
            .annotations
            .as_ref()
            .and_then(|a| a.open_world_hint),
        Some(true)
    );
    let mut seen = String::new();
    // The agent cannot redirect the stream (and with it the key).
    let redirect = call(
        &client,
        "start_streaming",
        json!({"url": "rtmp://attacker.example/app"}),
    )
    .await;
    assert_eq!(error_code(&redirect), "permission_denied");
    // Start, retrying while the test ingest is not listening yet.
    let deadline = Instant::now() + Duration::from_secs(10);
    let started = loop {
        let result = call(
            &client,
            "start_streaming",
            json!({"video_bitrate_kbps": 2000}),
        )
        .await;
        seen.push_str(&format!("{result:?}"));
        if result.is_error != Some(true) || Instant::now() >= deadline {
            break result;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Every attempt, not just the last, if starting never succeeded, and
    // why the ingest stopped listening if it did.
    if started.is_error != Some(false) {
        let state = if ingest.has_ended() {
            format!("the ingest ended: {:?}", ingest.join().error)
        } else {
            "the ingest is still waiting".to_owned()
        };
        panic!("start_streaming kept failing ({state}): {seen}");
    }
    assert!(
        ok(&started)["target"]
            .as_str()
            .is_some_and(|target| target.ends_with("<stream key>"))
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let status = call(&client, "get_status", json!({})).await;
    assert_eq!(ok(&status)["streaming_state"], "connected");
    let stopped = call(&client, "stop_streaming", json!({})).await;
    assert_eq!(ok(&stopped)["streaming"]["connects"], 1);
    seen.push_str(&format!("{status:?}{stopped:?}{tools:?}"));
    seen.push_str(&format!("{:?}", client.peer_info()));
    assert!(!seen.contains("SECRET"), "the key leaked to the agent");
    client.cancel().await.expect("close");
    let arrived = ingest.join();
    assert_eq!(arrived.video.expect("video").codec, "h264");
}

/// A termination signal (how clients usually stop stdio servers) still
/// finalizes an open recording. Driven over raw JSON-RPC lines, since the
/// SDK client kills its child on close.
// Sends SIGTERM with `kill`.
#[cfg(unix)]
#[test]
fn sigterm_finalizes_an_open_recording() {
    let _media = MEDIA.blocking_lock();
    use std::io::{BufRead, BufReader, Write};

    let dir = TempDir::new("sigterm");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_streamforge-mcp"))
        .args(["--allow", "sensitive", "--output-dir"])
        .arg(&dir.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let (lines_tx, lines) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().expect("stdout");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut send = |message: Value| {
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    };
    let response = |id: u64| loop {
        let line = lines
            .recv_timeout(Duration::from_secs(10))
            .expect("server answers");
        let message: Value = serde_json::from_str(&line).expect("json");
        if message["id"] == id {
            break message;
        }
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "sigterm-test", "version": "0"}}}),
    );
    assert!(response(1)["result"]["serverInfo"].is_object());
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "start_recording", "arguments": {"file_name": "signal.mkv"}}}),
    );
    let started = response(2);
    assert_eq!(started["result"]["isError"], false, "{started}");
    std::thread::sleep(Duration::from_millis(800));
    let terminated = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(terminated.success());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
    // Finalized: the container knows its duration (written at the end).
    ffmpeg_next::init().expect("init");
    let input = ffmpeg_next::format::input(&dir.0.join("signal.mkv")).expect("open recording");
    assert!(input.duration() > 0, "duration {}", input.duration());
}
