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
    AccessPolicy, Command, CommandDescriptor, CommandError, CommandOutput, ErrorCode, access_label,
    decode_streaming_to, params_schema, registry,
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
    /// A project directory is configured; without one, the project
    /// commands and `list_images` are not offered.
    pub projects: bool,
}

/// The commands that work in the project directory.
const PROJECT_COMMANDS: [&str; 5] = [
    "list_projects",
    "save_project",
    "load_project",
    "validate_project",
    "list_images",
];

pub struct StreamforgeMcp {
    engine: EngineHandle,
    config: Config,
    tools: Vec<Tool>,
}

impl StreamforgeMcp {
    pub fn new(engine: EngineHandle, config: Config) -> Self {
        let tools = registry()
            .iter()
            .filter(|descriptor| offered(&config, descriptor))
            .map(tool)
            .collect();
        Self {
            engine,
            config,
            tools,
        }
    }

    /// Validate and authorize a call; the engine only sees allowed commands.
    /// Decoding comes first, so a denial names the access this particular
    /// call needs (e.g. control for an `add_source` of color bars).
    fn prepare(&self, name: &str, arguments: JsonObject) -> Result<Command, CommandError> {
        let arguments = Value::Object(arguments);
        if !self.config.projects && PROJECT_COMMANDS.contains(&name) {
            return Err(CommandError::new(
                ErrorCode::PermissionDenied,
                format!(
                    "`{name}` is not available: no project directory is configured for this server"
                ),
            )
            .with_details(json!({ "command": name })));
        }
        let command = if name == "start_streaming" {
            let target = self.config.stream.clone().ok_or_else(|| {
                CommandError::new(
                    ErrorCode::PermissionDenied,
                    "`start_streaming` is not available: no streaming target is configured for \
                     this server",
                )
                .with_details(json!({ "command": name }))
            })?;
            decode_streaming_to(target, Some(&arguments))?
        } else {
            Command::decode(name, Some(&arguments))?
        };
        self.config.policy.authorize(&command)?;
        Ok(command)
    }
}

/// Whether to offer a command as a tool under `config`: if some call of it
/// is allowed, and (for streaming and projects) it is configured.
fn offered(config: &Config, descriptor: &CommandDescriptor) -> bool {
    config.policy.allows(descriptor.least_access())
        && (descriptor.name != "start_streaming" || config.stream.is_some())
        && (!PROJECT_COMMANDS.contains(&descriptor.name) || config.projects)
}

fn tool(descriptor: &CommandDescriptor) -> Tool {
    let name = descriptor.name;
    let mut schema = params_schema(name).unwrap_or_else(|| json!({"type": "object"}));
    let description = if name == "start_streaming" {
        // The target (URL and key) comes from the server's configuration.
        if let Some(properties) = schema["properties"].as_object_mut() {
            properties.remove("url");
            properties.remove("stream_key");
        }
        if let Some(required) = schema["required"].as_array_mut() {
            required.retain(|field| !matches!(field.as_str(), Some("url" | "stream_key")));
        }
        "Publish the media pipeline (active scene and audio mix) over RTMP (H.264 CBR + AAC) to \
         the target configured when this server was started; the URL and key cannot be \
         chosen here. Params: video_bitrate_kbps and video_encoder (default: the stream \
         profile's), reconnect_attempts (default 5), reconnect_delay_ms (default 1000). \
         Returns once connected; reconnects after errors."
            .to_owned()
    } else if descriptor.least_access() != descriptor.access {
        format!(
            "{} Needs {} access for some parameters (screen sources) and {} otherwise.",
            descriptor.summary,
            access_label(descriptor.access),
            access_label(descriptor.least_access())
        )
    } else {
        descriptor.summary.to_owned()
    };
    let input_schema: JsonObject = match schema {
        Value::Object(object) => object,
        _ => JsonObject::new(),
    };
    let read_only = descriptor.access == command_api::CommandAccess::ReadOnly;
    let annotations = ToolAnnotations::new()
        .read_only(read_only)
        .destructive(descriptor.destructive)
        .open_world(descriptor.open_world);
    Tool::new(name, description, Arc::new(input_schema)).with_annotations(annotations)
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
                access_label(self.config.policy.max())
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
        // Queue without blocking (a full queue is a `busy` error), then wait
        // for the reply on a blocking thread; at most the queue's worth of
        // calls wait at a time.
        let outcome = match self
            .prepare(&name, request.arguments.unwrap_or_default())
            .and_then(|command| self.engine.submit(command))
        {
            Ok(reply) => tokio::task::spawn_blocking(move || reply.wait())
                .await
                .unwrap_or_else(|err| {
                    Err(CommandError::new(
                        ErrorCode::Internal,
                        format!("the command task failed: {err}"),
                    ))
                }),
            Err(error) => Err(error),
        };
        Ok(result(outcome).into())
    }
}
