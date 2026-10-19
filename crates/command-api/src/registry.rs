//! Command registry: wire names, metadata, and parameter decoding.

use serde::Serialize;
use serde_json::{Value, json};

use crate::{Command, CommandError, ErrorCode};

/// Permission class of a command.
///
/// Adapters must require explicit authorization for anything other than
/// [`CommandAccess::ReadOnly`]; enforcement arrives with agent integration (M8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAccess {
    /// Inspects state without changing it.
    ReadOnly,
    /// Changes engine state, e.g. starts or stops an output.
    Control,
    /// Destructive or credential-related.
    Sensitive,
}

/// Static metadata for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandDescriptor {
    /// Stable `snake_case` wire name.
    pub name: &'static str,
    pub summary: &'static str,
    pub access: CommandAccess,
}

const GET_STATUS: CommandDescriptor = CommandDescriptor {
    name: "get_status",
    summary: "Report engine status.",
    access: CommandAccess::ReadOnly,
};

const LIST_COMMANDS: CommandDescriptor = CommandDescriptor {
    name: "list_commands",
    summary: "List commands supported by this engine.",
    access: CommandAccess::ReadOnly,
};

const COMMANDS: &[CommandDescriptor] = &[GET_STATUS, LIST_COMMANDS];

/// All commands known to this build, in a stable order.
pub fn registry() -> &'static [CommandDescriptor] {
    COMMANDS
}

/// Result of `list_commands`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandList {
    pub commands: Vec<CommandDescriptor>,
}

impl CommandList {
    pub fn all() -> Self {
        Self {
            commands: COMMANDS.to_vec(),
        }
    }
}

impl Command {
    /// Decode a command from its wire name and optional JSON parameters.
    ///
    /// Absent, `null`, and empty-object parameters are equivalent.
    pub fn decode(name: &str, params: Option<&Value>) -> Result<Self, CommandError> {
        match name {
            "get_status" => expect_no_params(name, params).map(|()| Self::GetStatus),
            "list_commands" => expect_no_params(name, params).map(|()| Self::ListCommands),
            _ => Err(CommandError::new(
                ErrorCode::UnknownCommand,
                format!("unknown command `{name}`"),
            )
            .with_details(json!({
                "command": name,
                "available": COMMANDS.iter().map(|d| d.name).collect::<Vec<_>>(),
            }))),
        }
    }

    pub fn descriptor(&self) -> &'static CommandDescriptor {
        match self {
            Self::GetStatus => &GET_STATUS,
            Self::ListCommands => &LIST_COMMANDS,
        }
    }

    pub fn name(&self) -> &'static str {
        self.descriptor().name
    }
}

fn expect_no_params(command: &str, params: Option<&Value>) -> Result<(), CommandError> {
    match params {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) if map.is_empty() => Ok(()),
        Some(Value::Object(map)) => Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("`{command}` takes no parameters"),
        )
        .with_details(json!({
            "command": command,
            "unexpected": map.keys().collect::<Vec<_>>(),
        }))),
        Some(other) => Err(CommandError::new(
            ErrorCode::InvalidParams,
            format!("`params` must be an object, got {}", json_type(other)),
        )
        .with_details(json!({ "command": command }))),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_names_are_unique_snake_case() {
        let mut seen = HashSet::new();
        for descriptor in registry() {
            assert!(
                seen.insert(descriptor.name),
                "duplicate {}",
                descriptor.name
            );
            assert!(
                descriptor
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} is not snake_case",
                descriptor.name
            );
        }
    }

    #[test]
    fn every_registered_command_decodes_to_its_descriptor() {
        for descriptor in registry() {
            let command = Command::decode(descriptor.name, None)
                .unwrap_or_else(|err| panic!("{} does not decode: {err}", descriptor.name));
            assert_eq!(command.descriptor(), descriptor);
        }
    }

    #[test]
    fn unknown_command_lists_available_commands() {
        let err = Command::decode("start_teleport", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownCommand);
        let details = err.details.expect("details");
        assert_eq!(details["command"], "start_teleport");
        assert_eq!(details["available"], json!(["get_status", "list_commands"]));
    }

    #[test]
    fn absent_null_and_empty_params_are_equivalent() {
        for params in [None, Some(&Value::Null), Some(&json!({}))] {
            assert_eq!(
                Command::decode("get_status", params),
                Ok(Command::GetStatus)
            );
        }
    }

    #[test]
    fn unexpected_params_are_rejected() {
        let err = Command::decode("get_status", Some(&json!({"verbose": true}))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert_eq!(
            err.details.expect("details")["unexpected"],
            json!(["verbose"])
        );
    }

    #[test]
    fn non_object_params_are_rejected() {
        let err = Command::decode("list_commands", Some(&json!([1]))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("array"), "{}", err.message);
    }

    #[test]
    fn command_list_serializes_with_access_class() {
        let value = serde_json::to_value(CommandList::all()).expect("serialize");
        assert_eq!(
            value["commands"][0],
            json!({
                "name": "get_status",
                "summary": "Report engine status.",
                "access": "read_only",
            })
        );
    }
}
