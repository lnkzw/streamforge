//! MCP adapter: agents call StreamForge commands as MCP tools.
//!
//! Every tool is a command from the registry, with its parameter schema
//! generated from the decoder. A call takes the same path as every other
//! adapter (`Command::decode`, then the engine's `CommandHandler`), after an
//! [`AccessPolicy`] check chosen by the person who starts the server:
//! tools above the allowed level are neither listed nor callable.
//!
//! Credentials never pass through the agent: `start_streaming` is offered
//! only for a target (URL and key) configured at startup, and its tool takes
//! neither a URL nor a key. Accepting a URL from the agent would let it
//! send the configured key to any server.

use std::sync::Arc;

use command_api::{
    AccessPolicy, Command, CommandAccess, CommandError, CommandOutput, ErrorCode, params_schema,
    registry,
};
use encoder_api::StreamTarget;
use engine::EngineHandle;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};

/// What the person starting the server allows.
pub struct Config {
    pub policy: AccessPolicy,
    /// The only streaming target agents may use (with its secret key).
    pub stream: Option<StreamTarget>,
}

pub struct StreamforgeMcp {
    engine: EngineHandle,
    config: Config,
    tools: Vec<Tool>,
}

/// Stands in for the configured target while decoding agent parameters.
const PLACEHOLDER_URL: &str = "rtmp://configured-target/app";

/// Parameters of `start_streaming` that only the configuration sets.
const CONFIGURED_STREAM_PARAMS: [&str; 2] = ["url", "stream_key"];

impl StreamforgeMcp {
    pub fn new(engine: EngineHandle, config: Config) -> Self {
        let tools = registry()
            .iter()
            .filter(|descriptor| offered(&config, descriptor.name, descriptor.access))
            .map(|descriptor| tool(descriptor.name, descriptor.summary, descriptor.access))
            .collect();
        Self {
            engine,
            config,
            tools,
        }
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Validate and authorize a call; the engine only sees allowed commands.
    fn prepare(&self, name: &str, arguments: JsonObject) -> Result<Command, CommandError> {
        if !self.tools.iter().any(|tool| tool.name == name) {
            return Err(self.not_offered(name));
        }
        let command = if name == "start_streaming" {
            self.streaming_command(arguments)?
        } else {
            Command::decode(name, Some(&Value::Object(arguments)))?
        };
        self.config.policy.authorize(&command)?;
        Ok(command)
    }

    /// `start_streaming` to the configured target: the agent may set bit
    /// rate, reconnects and sources, never the URL or the key.
    fn streaming_command(&self, mut arguments: JsonObject) -> Result<Command, CommandError> {
        let target = self
            .config
            .stream
            .clone()
            .ok_or_else(|| self.not_offered("start_streaming"))?;
        if let Some(field) = CONFIGURED_STREAM_PARAMS
            .iter()
            .find(|field| arguments.contains_key(**field))
        {
            return Err(CommandError::new(
                ErrorCode::PermissionDenied,
                format!(
                    "`{field}` is set by the person running this server; streaming goes only to \
                     the configured target"
                ),
            ));
        }
        // Decode with a placeholder target to validate the agent's
        // parameters, then use the configured target (which holds the key).
        arguments.insert("url".into(), PLACEHOLDER_URL.into());
        let mut command = Command::decode("start_streaming", Some(&Value::Object(arguments)))?;
        if let Command::StartStreaming(request) = &mut command {
            request.target = target;
        }
        Ok(command)
    }

    fn not_offered(&self, name: &str) -> CommandError {
        match registry().iter().find(|descriptor| descriptor.name == name) {
            Some(descriptor) => {
                let reason = if name == "start_streaming" && self.config.stream.is_none() {
                    "no streaming target is configured for this server".to_owned()
                } else {
                    format!(
                        "it needs {} access, but this server allows only {}",
                        level(descriptor.access),
                        level(self.config.policy.max())
                    )
                };
                CommandError::new(
                    ErrorCode::PermissionDenied,
                    format!("`{name}` is not available: {reason}"),
                )
                .with_details(json!({ "command": name }))
            }
            None => CommandError::new(ErrorCode::UnknownCommand, format!("unknown tool `{name}`")),
        }
    }
}

/// Whether to offer a command as a tool under `config`.
fn offered(config: &Config, name: &str, access: CommandAccess) -> bool {
    // `add_source` needs only control for color bars and solids (see
    // `Command::required_access`); screen sources are checked per call.
    let least = if name == "add_source" {
        CommandAccess::Control
    } else {
        access
    };
    config.policy.allows(least) && (name != "start_streaming" || config.stream.is_some())
}

fn tool(name: &'static str, summary: &'static str, access: CommandAccess) -> Tool {
    let mut schema = params_schema(name).unwrap_or_else(|| json!({"type": "object"}));
    let mut description = summary.to_owned();
    if name == "start_streaming" {
        // URL and key come from the server's configuration.
        if let Some(properties) = schema["properties"].as_object_mut() {
            for field in CONFIGURED_STREAM_PARAMS {
                properties.remove(field);
            }
        }
        if let Some(required) = schema["required"].as_array_mut() {
            required
                .retain(|field| !CONFIGURED_STREAM_PARAMS.contains(&field.as_str().unwrap_or("")));
        }
        description.push_str(
            " Over MCP the target (URL and key) is the one configured when the server was \
             started; the tool takes neither.",
        );
    }
    if name == "add_source" {
        description.push_str(" Screen sources need sensitive access; others need control.");
    }
    let input_schema: JsonObject = match schema {
        Value::Object(object) => object,
        _ => JsonObject::new(),
    };
    let annotations = match access {
        CommandAccess::ReadOnly => ToolAnnotations::new().read_only(true),
        _ => ToolAnnotations::new()
            .read_only(false)
            .destructive(matches!(name, "delete_scene" | "remove_source"))
            .open_world(name == "start_streaming"),
    };
    Tool::new(name, description, Arc::new(input_schema)).with_annotations(annotations)
}

fn level(access: CommandAccess) -> &'static str {
    command_api::access_label(access)
}

/// Error codes on this adapter's wire, mapped explicitly so internal
/// renames cannot change what agents see.
fn wire_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::UnknownCommand => "unknown_command",
        ErrorCode::InvalidParams => "invalid_params",
        ErrorCode::InvalidState => "invalid_state",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Timeout => "timeout",
        ErrorCode::BackendError => "backend_error",
        ErrorCode::Internal => "internal",
    }
}

fn result(outcome: Result<CommandOutput, CommandError>) -> CallToolResult {
    match outcome.and_then(|output| {
        serde_json::to_value(&output).map_err(|err| {
            CommandError::new(
                ErrorCode::Internal,
                format!("cannot encode the result: {err}"),
            )
        })
    }) {
        // Every command result is a JSON object.
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({
            "code": wire_code(error.code),
            "message": error.message,
            "details": error.details,
        })),
    }
}

impl ServerHandler for StreamforgeMcp {
    fn get_info(&self) -> ServerConfig {
        let streaming = match &self.config.stream {
            Some(target) => format!("Streaming goes only to {}.", target.redacted()),
            None => "No streaming target is configured.".to_owned(),
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "streamforge",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "StreamForge: scenes, audio mixer, recording and streaming through typed \
                 commands. This server allows {} commands; tools above that level are not \
                 offered (the person running the server chooses the level with --allow). \
                 {streaming} Start with get_status and list_scenes.",
                level(self.config.policy.max())
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        if !registry().iter().any(|descriptor| descriptor.name == name) {
            return Err(McpError::invalid_params(
                format!("unknown tool `{name}`"),
                None,
            ));
        }
        let outcome = match self.prepare(&name, request.arguments.unwrap_or_default()) {
            Ok(command) => {
                let engine = self.engine.clone();
                tokio::task::spawn_blocking(move || engine.call(command))
                    .await
                    .unwrap_or_else(|err| {
                        Err(CommandError::new(
                            ErrorCode::Internal,
                            format!("the command task failed: {err}"),
                        ))
                    })
            }
            Err(error) => Err(error),
        };
        Ok(result(outcome).into())
    }
}
